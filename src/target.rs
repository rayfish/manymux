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
use manymux::client::switch::Located;
use manymux::hosts::{Hosts, LOCAL, Target, is_this_machine, this_machine};
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
        let mut hosts: Vec<String> = self
            .answered
            .iter()
            .filter(|host| !is_this_machine(host))
            .cloned()
            .collect();
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
    if let Some(host) = target.host {
        return Ok(Located {
            host,
            session: target.session,
        });
    }

    // Nearly always here, and asking is one round trip on a local socket.
    let here = sessions_on(socket, LOCAL).await.unwrap_or_default();
    if here.iter().any(|session| session.name == target.session) {
        return Ok(Located {
            host: this_machine().to_string(),
            session: target.session,
        });
    }

    let listing = everywhere(socket).await?;
    let mut hosts: Vec<String> = listing
        .sessions
        .iter()
        .filter(|hosted| hosted.session.name == target.session)
        .map(|hosted| hosted.host.clone())
        .collect();
    hosts.dedup();

    match hosts.len() {
        1 => Ok(Located {
            host: hosts.remove(0),
            session: target.session,
        }),
        0 if bare == Bare::OrMachine && names_a_machine(&target.session) => {
            on_that_machine(&listing, &target.session)
        }
        0 => bail!("no session named {}; see `mm ls`", target.session),
        // Two machines can each have a `build`. Say which, rather than guessing.
        _ => bail!(
            "{} is on more than one machine ({}); say which, like `{}/{}`",
            target.session,
            hosts.join(", "),
            hosts[0],
            target.session
        ),
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
}
