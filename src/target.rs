//! Working out which machine and which session someone meant.
//!
//! Everything between the words typed at a shell and a `host/name` worth acting
//! on: asking every watched machine at once and keeping the ones that did not
//! answer separate from the ones with nothing to say, deciding whether a bare
//! word is a session, a machine or a command, and telling this machine's node
//! which machines turned out to be up.
//!
//! Kept apart from the commands themselves because it answers a different
//! question. `mm attach`, `mm kill` and `mm rename` all want the same thing
//! from a word, and only differ in what they do once they have it.

use std::path::Path;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tokio::time::timeout;
use tracing::debug;

use manymux::client::Stream;
use manymux::client::groups::Groups;
use manymux::client::switch::Located;
use manymux::hosts::{Hosts, LOCAL, Named, Target, is_this_machine, this_machine};
use manymux::proto::{HostedSession, Request, Response, SessionInfo};

use crate::open;

/// Sessions found, and machines that could not be asked. One being asleep must
/// not hide the others.
#[derive(Default)]
pub(crate) struct Listing {
    pub(crate) sessions: Vec<HostedSession>,
    pub(crate) unreachable: Vec<Unreachable>,
    /// Every machine that answered, whether or not it had anything to say. Kept
    /// separately because a machine with no sessions on it puts nothing in
    /// `sessions` and is every bit as reached as a busy one.
    answered: Vec<String>,
}

/// A machine that could not be reached, and why.
pub(crate) struct Unreachable {
    pub(crate) host: String,
    pub(crate) error: String,
}

impl Listing {
    pub(crate) fn add(&mut self, host: &str, found: Result<Vec<SessionInfo>>) {
        match found {
            Ok(sessions) => {
                self.answered.push(host.to_string());
                self.sessions
                    .extend(sessions.into_iter().map(|session| HostedSession {
                        host: host.to_string(),
                        session,
                    }))
            }
            Err(e) => self.unreachable.push(Unreachable {
                host: host.to_string(),
                error: format!("{e:#}"),
            }),
        }
    }

    /// The machines worth telling this machine's node about, so it can
    /// resubscribe to any it had given up on. This machine is not one of them:
    /// it is never watched, being where the watching happens.
    pub(crate) fn reached(&self) -> Vec<String> {
        let mut hosts = self.answering();
        hosts.retain(|host| !is_this_machine(host));
        hosts
    }

    /// Every machine that answered, this one included.
    ///
    /// Which is what pruning a group wants and [`Self::reached`] is not: that
    /// one leaves this machine out because this machine is never watched, and
    /// pruning fed the same list kept every local member of every group
    /// forever. A group that had ended on the machine you actually work on
    /// stayed in `groups.toml`, and its name stayed in the completions, with
    /// nothing on any screen to say so: every view of a group counts the
    /// sessions the listing knows about, so a dead member is invisible in all
    /// of them.
    pub(crate) fn answering(&self) -> Vec<String> {
        let mut hosts = self.answered.clone();
        hosts.sort();
        hosts.dedup();
        hosts
    }
}

/// Tell this machine's node which machines just answered, so it can subscribe
/// again to any it had given up on.
///
/// Nothing here is worth failing a command over, so every way it can go wrong
/// is the same as it going right: no node running here (the common case on a
/// machine used only to reach others), a node too old to know the request, or a
/// socket that has gone. There is nothing to do about any of them, and nothing
/// the person who typed `mm ls` would want said about it.
pub(crate) async fn note_reached(socket: &Path, hosts: Vec<String>) {
    if hosts.is_empty() {
        return;
    }
    let told = async {
        Stream::local(socket)
            .await?
            .call(&Request::Reached { hosts })
            .await
    };
    if let Err(e) = told.await {
        debug!("could not tell the node what answered: {e:#}");
    }
}

/// One machine's answer, kept with which machine gave it.
struct Asked {
    host: String,
    found: Result<Vec<SessionInfo>>,
}

/// How long one machine may take over a full listing before it is reported as
/// not answering.
///
/// Without this a single machine that is asleep or off the network holds up
/// every other machine's answer for as long as the kernel spends on a TCP
/// connect, which is minutes and once per address the name resolves to. Naming
/// a machine yourself still waits as long as it takes: you asked about that one
/// and an error about it is the answer. It is a fan-out that needs the bound.
///
/// Generous enough for a cold ssh handshake through a `ProxyCommand` and a node
/// starting at the far end, and short enough that a listing stays something you
/// wait for rather than something you go away from.
const HOST_DEADLINE: Duration = Duration::from_secs(5);

/// Sessions on this machine and on every watched one, asked all at once.
pub(crate) async fn everywhere(socket: &Path) -> Result<Listing> {
    let mut listing = Listing::default();

    // No node here is ordinary on a machine you only use to reach others, so it
    // is not worth a complaint.
    match sessions_on(socket, LOCAL).await {
        Ok(sessions) => listing.add(this_machine(), Ok(sessions)),
        Err(e) => debug!("no node here: {e:#}"),
    }

    let mut asked = tokio::task::JoinSet::new();
    for host in Hosts::load()?.names() {
        let socket = socket.to_path_buf();
        asked.spawn(async move {
            // Giving up drops the query, and with it the ssh carrying it, so a
            // machine that never answers leaves nothing behind.
            let found = match timeout(HOST_DEADLINE, sessions_on(&socket, &host)).await {
                Ok(found) => found,
                Err(_) => Err(anyhow!("no answer in {}s", HOST_DEADLINE.as_secs())),
            };
            Asked { host, found }
        });
    }
    while let Some(answer) = asked.join_next().await {
        match answer {
            Ok(Asked { host, found }) => listing.add(&host, found),
            Err(e) => debug!("a host query did not finish: {e}"),
        }
    }
    note_reached(socket, listing.reached()).await;

    // By machine first, which is what makes each one's sessions a run in the
    // table and in the switch keys' cycle, then oldest first within it.
    listing.sessions.sort_by(|a, b| {
        (&a.host, a.session.started, &a.session.name).cmp(&(
            &b.host,
            b.session.started,
            &b.session.name,
        ))
    });
    listing.unreachable.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(listing)
}

pub(crate) async fn sessions_on(socket: &Path, host: &str) -> Result<Vec<SessionInfo>> {
    // No node here means no sessions here, which is an answer rather than a
    // failure. Starting one just to be told that would be rude.
    if is_this_machine(host) && !socket.exists() {
        // Unless the sessions are all sitting in a node an older build left
        // somewhere else, in which case an empty table is a lie.
        manymux::node::note_a_node_left_behind(socket).await;
        return Ok(Vec::new());
    }
    match open(socket, host).await?.call(&Request::List).await? {
        Response::Sessions(sessions) => Ok(sessions),
        other => bail!("unexpected response to list: {other:?}"),
    }
}

/// What a bare word is allowed to mean besides a session name.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bare {
    /// A session and nothing else. A word that happens to name a machine is a
    /// session name that is not running.
    Session,
    /// A machine too, standing for whatever is running on it. Only for going
    /// somewhere: picking a session out of a list is fine to attach to and not
    /// fine to kill or rename, where the wrong guess is not undoable.
    OrMachine,
}

/// Work out which machine a target is on.
///
/// `host/session` says so outright. A bare name is looked for here first, and
/// then across every watched machine, so `mm attach api` finds the session
/// wherever you left it without having to remember which machine that was.
pub(crate) async fn locate(socket: &Path, target: &str, bare: Bare) -> Result<Located> {
    let target = Target::parse(target)?;

    // A group names more than one session, and `mm kill` and `mm rename` act on
    // exactly one and cannot be undone. Refused for the same reason a bare
    // machine name is only accepted for going somewhere.
    let group = target
        .session
        .group()
        .or_else(|| target.host.as_ref().and_then(Named::group));
    if let Some(group) = group {
        if bare == Bare::Session {
            bail!("{group} is a group, which names more than one session; name the one you mean");
        }
        // The listing has to be fetched whatever else is known: a group spans
        // machines, so there is no telling which ones it is on without asking.
        let listing = everywhere(socket).await?;
        note_reached(socket, listing.reached()).await;
        let groups = Groups::load()?;
        let (host, name) = match (&target.host, &target.session) {
            (Some(Named::Session(host)), Named::Group(_)) => (Some(host.as_str()), None),
            (Some(Named::Group(_)), Named::Session(name)) => (None, Some(name.as_str())),
            _ => (None, None),
        };
        return in_group(&groups, &listing.sessions, group, host, name);
    }

    let wanted = target.session.name().to_string();
    if let Some(Named::Session(host)) = target.host {
        return Ok(Located {
            host,
            session: wanted,
        });
    }

    // Nearly always here, and asking is one round trip on a local socket.
    let here = sessions_on(socket, LOCAL).await.unwrap_or_default();
    if here.iter().any(|session| session.name == wanted) {
        return Ok(Located {
            host: this_machine().to_string(),
            session: wanted,
        });
    }

    let listing = everywhere(socket).await?;
    let mut hosts: Vec<String> = listing
        .sessions
        .iter()
        .filter(|hosted| hosted.session.name == wanted)
        .map(|hosted| hosted.host.clone())
        .collect();
    hosts.dedup();

    match hosts.len() {
        1 => Ok(Located {
            host: hosts.remove(0),
            session: wanted,
        }),
        0 if bare == Bare::OrMachine && names_a_machine(&wanted) => {
            on_that_machine(&listing, &wanted)
        }
        0 => bail!("no session named {wanted}; see `mm ls`"),
        // Two machines can each have a `build`. Say which, rather than guessing.
        _ => bail!(
            "{wanted} is on more than one machine ({}); say which, like `{}/{wanted}`",
            hosts.join(", "),
            hosts[0],
        ),
    }
}

/// A session inside a group, optionally narrowed to one machine or one name.
///
/// The listing arrives ordered by machine and then by when each session was
/// opened, so the first row that matches is the oldest one in the group, which
/// is the same rule the host keys land by: first as `mm ls` prints it.
fn in_group(
    groups: &Groups,
    listing: &[HostedSession],
    group: &str,
    host: Option<&str>,
    name: Option<&str>,
) -> Result<Located> {
    let found = listing.iter().find(|hosted| {
        groups.group_of(&hosted.host, &hosted.session) == Some(group)
            && host.is_none_or(|want| hosted.host == want)
            && name.is_none_or(|want| hosted.session.name == want)
    });
    if let Some(hosted) = found {
        return Ok(Located::new(&hosted.host, &hosted.session.name));
    }

    // Which of the two ways it missed, since they want different answers: a
    // group nobody is running is a name to correct, while a session that is
    // simply not in one is a session you reach without naming the group.
    let live = groups.tally(listing);
    if !live.iter().any(|(g, _)| g == group) {
        let names: Vec<&str> = live.iter().map(|(g, _)| g.as_str()).collect();
        if names.is_empty() {
            bail!("no group named {group}; see `mm groups`");
        }
        bail!("no group named {group}; there is {}", names.join(", "));
    }
    match (host, name) {
        (Some(host), _) => bail!("nothing from {group} is running on {host}"),
        (_, Some(name)) => bail!("no session named {name} in {group}"),
        _ => bail!("no group named {group}; see `mm groups`"),
    }
}

/// Whether a word names a machine rather than a session.
///
/// Only this machine and one already being watched count. A word nobody has
/// added is a mistyped session name, and treating it as a machine would turn
/// every typo into an ssh connection to a host that does not exist.
fn names_a_machine(word: &str) -> bool {
    is_this_machine(word) || Hosts::load().is_ok_and(|hosts| hosts.has(word))
}

/// The first session on a machine named on its own.
///
/// `mm a devbox` reads as "put me on devbox", and on a machine with one session
/// naming it as well is repeating a lookup you have just done. First is first as
/// `mm ls` prints it, so what you get is the row you were looking at.
fn on_that_machine(listing: &Listing, machine: &str) -> Result<Located> {
    let here = is_this_machine(machine);
    let found = listing
        .sessions
        .iter()
        .find(|hosted| hosted.host == machine || (here && is_this_machine(&hosted.host)));
    if let Some(hosted) = found {
        return Ok(Located {
            host: hosted.host.clone(),
            session: hosted.session.name.clone(),
        });
    }
    // A machine that could not be asked has not said it is empty, and saying it
    // is would send someone looking for sessions that are still there.
    if let Some(down) = listing.unreachable.iter().find(|had| had.host == machine) {
        bail!("{}: {}", down.host, down.error);
    }
    bail!("nothing is running on {machine}; `mm n {machine} <command>` starts something there")
}

pub(crate) fn qualified(host: &str, name: &str) -> String {
    if is_this_machine(host) {
        name.to_string()
    } else {
        format!("{host}/{name}")
    }
}

/// Where a `mm new` should run, and what it should run there.
pub(crate) struct Started {
    pub(crate) host: String,
    pub(crate) command: Vec<String>,
}

/// Split `[host] [command...]`.
///
/// The first argument is what to run if it is a command, and where to run it
/// otherwise. No registration needed: ssh does not make you declare a host
/// before using it, so neither does this, and an ssh destination that does not
/// exist gets ssh's own error rather than one of ours.
pub(crate) fn where_to_start(mut args: Vec<String>) -> Result<Started> {
    let is_host = match args.first() {
        Some(first) => !runnable(first),
        None => false,
    };
    Ok(Started {
        host: if is_host {
            args.remove(0)
        } else {
            LOCAL.to_string()
        },
        command: args,
    })
}

/// Whether this looks like something to run rather than somewhere to run it.
///
/// A path or a shell snippet is taken at face value; a bare word has to be on
/// `PATH`, which is what separates `mm new claude` from `mm new gpu-box`.
fn runnable(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    if program.contains(|c: char| c.is_whitespace() || "|&;<>()$`\\\"'".contains(c)) {
        return true;
    }
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|dir| dir.join(program).is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            title: name.to_string(),
            command: "zsh".into(),
            pid: 1,
            size: manymux::proto::Size::new(80, 24),
            attached: 0,
            idle: 0,
            bells: 0,
            started: std::time::SystemTime::UNIX_EPOCH,
        }
    }

    /// What the node is told answered, so it can resubscribe to a machine it
    /// gave up on. A machine that did not answer must not be in it, or giving
    /// up would be undone by the very listing that proved the host is still
    /// unreachable.
    #[test]
    fn only_the_machines_that_answered_count_as_reached() {
        let mut listing = Listing::default();
        listing.add(this_machine(), Ok(vec![session("here")]));
        listing.add("gpu-box", Ok(vec![session("build")]));
        // Reachable and idle. Nothing in the table, and still reached.
        listing.add("api", Ok(Vec::new()));
        listing.add("asleep", Err(anyhow!("no answer in 5s")));

        assert_eq!(listing.reached(), vec!["api", "gpu-box"]);
    }

    /// And what a group is pruned against, which is the same list with this
    /// machine back in it. Fed `reached`, pruning kept every local member of
    /// every group forever: this machine is never in that list, and a member
    /// whose machine did not answer is a member nothing has been said about.
    #[test]
    fn pruning_counts_this_machine_among_the_ones_that_answered() {
        let mut listing = Listing::default();
        listing.add(this_machine(), Ok(vec![session("here")]));
        listing.add("gpu-box", Ok(vec![session("build")]));
        listing.add("asleep", Err(anyhow!("no answer in 5s")));

        let answering = listing.answering();
        assert!(answering.contains(&this_machine().to_string()));
        assert!(answering.contains(&"gpu-box".to_string()));
        assert!(!answering.contains(&"asleep".to_string()));
    }

    fn at(name: &str, pid: u32, started: u64) -> SessionInfo {
        SessionInfo {
            pid,
            started: std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(started),
            ..session(name)
        }
    }

    /// Two machines, one group across both, and a `build` on each so that
    /// narrowing by name has something to get wrong.
    fn world() -> (Groups, Vec<HostedSession>) {
        let build = at("build", 1, 1000);
        let api = at("api", 2, 1001);
        let train = at("train", 3, 1002);
        let other = at("build", 4, 1003);
        let mut groups = Groups::default();
        groups.assign("pi", "box", &build);
        groups.assign("pi", "gpu-box", &train);
        let listing = vec![
            HostedSession {
                host: "box".into(),
                session: build,
            },
            HostedSession {
                host: "box".into(),
                session: api,
            },
            HostedSession {
                host: "gpu-box".into(),
                session: train,
            },
            HostedSession {
                host: "gpu-box".into(),
                session: other,
            },
        ];
        (groups, listing)
    }

    /// The oldest session in the group, which is the rule the host keys land
    /// by: first as `mm ls` prints it, rather than a second rule to learn.
    #[test]
    fn a_group_lands_on_the_oldest_session_in_it() {
        let (groups, listing) = world();
        let found = in_group(&groups, &listing, "pi", None, None).unwrap();
        assert_eq!(found, Located::new("box", "build"));
    }

    #[test]
    fn a_group_narrowed_to_a_machine_stays_on_it() {
        let (groups, listing) = world();
        let found = in_group(&groups, &listing, "pi", Some("gpu-box"), None).unwrap();
        assert_eq!(found, Located::new("gpu-box", "train"));
    }

    /// Both machines have a `build`; only one of them is in `pi`.
    #[test]
    fn a_group_picks_the_session_of_that_name_that_is_in_it() {
        let (groups, listing) = world();
        let found = in_group(&groups, &listing, "pi", None, Some("build")).unwrap();
        assert_eq!(found, Located::new("box", "build"));
    }

    #[test]
    fn a_group_nobody_is_running_says_which_ones_exist() {
        let (groups, listing) = world();
        let said = format!(
            "{:#}",
            in_group(&groups, &listing, "nope", None, None).unwrap_err()
        );
        assert!(said.contains("no group named nope"), "{said}");
        assert!(said.contains("pi"), "{said}");
    }

    #[test]
    fn a_session_that_is_not_in_the_group_is_not_found_through_it() {
        let (groups, listing) = world();
        let said = format!(
            "{:#}",
            in_group(&groups, &listing, "pi", None, Some("api")).unwrap_err()
        );
        assert!(said.contains("no session named api in pi"), "{said}");
    }

    #[test]
    fn a_group_with_nothing_on_the_machine_named_says_so() {
        let (groups, listing) = world();
        let said = format!(
            "{:#}",
            in_group(&groups, &listing, "pi", Some("far"), None).unwrap_err()
        );
        assert!(said.contains("nothing from pi is running on far"), "{said}");
    }
}
