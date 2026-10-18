//! The checkpoint commands: `mm checkpoint save`, `restore` and `show`.
//!
//! What a save writes down and what it leaves out is decided in
//! `client::checkpoint`, where it can be tested without a fleet. What is here
//! is the asking (the listing, the snapshot, `/proc` read over ssh) and the
//! words each answer comes to.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Result, bail};
use tracing::debug;

use manymux::client::attach;
use manymux::client::checkpoint::{self, Checkpoint, Kept};
use manymux::client::groups::Groups;
use manymux::client::switch::Located;
use manymux::hosts::{is_this_machine, qualified, same_machine};
use manymux::proto::{Doing, HostedSession, Request, Response};
use manymux::{shell, style, term};

use crate::target::{Listing, Unreachable, everywhere, note_reached};
use crate::{FAILED, OK, open, open_or_start};

/// What a save managed to write down.
///
/// Both numbers, rather than an exit code, because `--keep-sessions` has to
/// weigh them against something an exit code cannot carry: the count of
/// sessions that made the restart worth asking about in the first place.
pub(crate) struct Saved {
    /// Entries written for the machines this save asked about.
    pub(crate) written: usize,
    /// Sessions those machines are running that could not be written down.
    pub(crate) lost: usize,
}

impl Saved {
    pub(crate) fn code(&self) -> u8 {
        // A checkpoint that quietly covers half of what is running is the thing
        // to find out about before restarting anything, not after.
        if self.lost > 0 { FAILED } else { OK }
    }
}

/// `mm checkpoint save`: write down what every session is doing.
///
/// The order of the two questions matters. The listing comes first and is what
/// the file is built from, because it is the listing that carries the pid and
/// the start time a group is keyed on, and its order is the order every screen
/// shows sessions in. What each session is *doing* is asked separately and
/// matched back by name.
pub(crate) async fn save_checkpoint(socket: &Path, host: Option<String>) -> Result<Saved> {
    // Read before anything is asked of anybody. One broken by the hand-editing
    // this invites would otherwise be replaced by whatever this save happens to
    // cover, which is the one thing carrying over exists to prevent, and
    // `restore` already refuses to guess at the same file. A fleet is not worth
    // fanning out to for a save that cannot be written.
    let before = Checkpoint::load()?.sessions;
    let listing = everywhere(socket).await?;
    let groups = Groups::load().unwrap_or_default();
    let answering = listing.answering();
    let wanted = |at: &str| host.as_deref().is_none_or(|only| same_machine(only, at));

    // Before anything is written, since a machine named that nothing answers
    // to is a typo far more often than it is a machine with no sessions, and
    // reporting it after the file has been rewritten leaves `mm checkpoint
    // show` calling an untouched file freshly saved.
    if let Some(only) = &host
        && !answering.iter().any(|at| same_machine(only, at))
    {
        bail!("no machine answering to {only}");
    }

    let mut doing = HashMap::new();
    let mut refused = Vec::new();
    for at in &answering {
        if !wanted(at) {
            continue;
        }
        match what_is_doing(socket, at, &listing).await {
            Ok(answers) => {
                doing.extend(answers.into_iter().map(|d| (Located::new(at, &d.name), d)));
            }
            Err(e) => refused.push(Unreachable {
                host: at.clone(),
                error: format!("{e:#}"),
            }),
        }
    }
    let silent: Vec<String> = refused.iter().map(|host| host.host.clone()).collect();

    // What to write, which is a judgement on each session rather than anything
    // this end has left to ask: see `checkpoint::taking`.
    //
    // `ours` is the session this command was typed in, which is running this
    // command. Recorded as it stands, it would come back as `mm checkpoint
    // save`, and a restore would then rewrite the file it is walking. Found by
    // the session leader's pid rather than by `MM_SESSION`: that is stamped
    // into the environment once at spawn and a rename cannot reach back into
    // it, so a session renamed since would go unrecognised and record exactly
    // the thing this is here to avoid.
    let taken = checkpoint::take(checkpoint::Taking {
        sessions: &listing.sessions,
        only: host.as_deref(),
        answering: &answering,
        refused: &silent,
        doing: &doing,
        groups: &groups,
        ours: manymux::foreground::our_session(),
        before,
    });

    for dropped in &taken.dropped {
        eprintln!("mm: {}", left_out(dropped));
    }
    for both in &taken.shared {
        eprintln!(
            "{} {} and {} are both in {}, so both come back on the same \
             conversation.\n  Edit {} to name one of them, or leave it: the \
             newest wins.",
            style::amber("!"),
            style::bold(&both.first),
            style::bold(&both.second),
            both.cwd,
            Checkpoint::path().display()
        );
    }
    if taken.kept_from_before > 0 {
        println!(
            "  {}",
            style::amber(&format!(
                "{} of those kept the entry an earlier save wrote, which \
                 may name a directory they have since left",
                taken.kept_from_before
            ))
        );
    }

    let written = taken.written;
    let machines = taken.machines;
    Checkpoint {
        taken: Checkpoint::now(),
        sessions: taken.sessions,
    }
    .save()?;
    if taken.carried > 0 {
        println!(
            "  {}",
            style::faint(&if taken.carried == 1 {
                "1 session on a machine this did not ask about is still in the file".to_string()
            } else {
                format!(
                    "{} sessions on machines this did not ask about are still in the file",
                    taken.carried
                )
            })
        );
    }

    for host in &refused {
        // The node's own complaint, which names the version it is running: a
        // machine that has never been restarted since this was added is the
        // ordinary reason, and the one thing it cannot checkpoint is the
        // restart that would fix it.
        eprintln!("mm: {}: {}", host.host, host.error);
    }
    println!(
        "{} wrote {written} session{} on {machines} machine{} to {}",
        style::green("✓"),
        if written == 1 { "" } else { "s" },
        if machines == 1 { "" } else { "s" },
        style::bold(&Checkpoint::path().display().to_string())
    );
    Ok(Saved {
        written,
        lost: taken.lost,
    })
}

/// The one line said about a session a save could not write down.
///
/// The reason is the library's and the words are this end's, which is why this
/// is a match rather than a `Display` on [`checkpoint::Unsaid`]: a name and a
/// machine fall either side of the verb depending on which rule fired, and the
/// line is addressed to whoever just asked for a checkpoint.
fn left_out(dropped: &checkpoint::Dropped) -> String {
    let (host, name) = (&dropped.at.host, &dropped.at.session);
    match dropped.why {
        checkpoint::Unsaid::NothingSaid => format!("{host} said nothing about {name}"),
        checkpoint::Unsaid::NoDirectory => {
            format!("{host} cannot say where {name} is, so it is left out")
        }
        checkpoint::Unsaid::NoCommand => {
            format!("{host} cannot say what {name} is running, so it is left out")
        }
        checkpoint::Unsaid::Changed => {
            format!("{host} changed under the question, so {name} is left out")
        }
        checkpoint::Unsaid::StillStarting => format!(
            "{name} on {host} is still starting up, so it is left out; ask again in \
             a moment"
        ),
    }
}

/// What every session on one machine is doing.
///
/// Three ways to the same answer, and the two that do not ask the node are not
/// optimisations. A node only learns to answer `Request::Snapshot` in the build
/// that added it, so on any machine the *first* checkpoint is refused by the
/// very node whose sessions are worth saving, and restarting it to fix that is
/// the thing the checkpoint exists to make safe. On this machine `/proc` is
/// right there. On another, the same files are read over the ssh that is
/// already open, which needs nothing on the far side to have heard of
/// checkpoints, or of manymux at all.
async fn what_is_doing(socket: &Path, host: &str, listing: &Listing) -> Result<Vec<Doing>> {
    let here: Vec<&HostedSession> = listing
        .sessions
        .iter()
        .filter(|hosted| hosted.host == host)
        .collect();

    if is_this_machine(host) {
        return Ok(here
            .iter()
            .map(|hosted| {
                let session = &hosted.session;
                Doing::new(
                    session.name.clone(),
                    session.pid,
                    manymux::foreground::of(session.pid),
                )
            })
            .collect());
    }

    match open(socket, host)
        .await?
        .request(&Request::Snapshot)
        .await?
    {
        Response::Snapshot(answers) => Ok(answers),
        // The refusal of a node that predates the question, which is an answer
        // rather than a failure: it says to go and read the files instead.
        Response::Error(said) => {
            debug!("{host} cannot be asked what it is doing ({said}); reading /proc over ssh");
            over_ssh(host, &here).await
        }
        other => bail!("unexpected response to a snapshot: {other:?}"),
    }
}

/// Read what a machine is doing out of its `/proc`, over ssh.
///
/// Two passes, because which process to describe is decided *here*: the first
/// brings back each session leader's `stat`, this end works out what is in
/// front of its terminal, and the second brings back that process's directory
/// and argv. Two round trips rather than one is the price of the far side
/// deciding nothing, and ssh shares one connection per machine so the second
/// costs a message rather than a handshake.
///
/// A machine that cannot answer at all leaves every session it holds without a
/// directory, which is reported and counted against the save the way any other
/// undescribable session is.
async fn over_ssh(host: &str, here: &[&HostedSession]) -> Result<Vec<Doing>> {
    let leaders: Vec<u32> = here.iter().map(|hosted| hosted.session.pid).collect();
    if leaders.is_empty() {
        return Ok(Vec::new());
    }
    let stats = read_over_ssh(host, &leaders, checkpoint::Want::Stat).await?;
    // Whose directory and argv to ask for, decided by the same rule the local
    // path uses, on the same bytes.
    let fronts: Vec<u32> = leaders
        .iter()
        .map(|&leader| checkpoint::in_front_of(leader, &stats))
        .collect();
    let cwds = read_over_ssh(host, &fronts, checkpoint::Want::Cwd).await?;
    let argvs = read_over_ssh(host, &fronts, checkpoint::Want::Cmdline).await?;

    Ok(here
        .iter()
        .zip(&fronts)
        .map(|(hosted, &front)| checkpoint::described(&hosted.session, front, &cwds, &argvs))
        .collect())
}

/// One pass of [`over_ssh`]: the same `/proc` file for each of these pids.
async fn read_over_ssh(
    host: &str,
    pids: &[u32],
    want: checkpoint::Want,
) -> Result<checkpoint::ReadBack> {
    let out = manymux::ssh::ask(host, &checkpoint::read_proc(pids, want)).await?;
    Ok(checkpoint::read_back(&out))
}

/// `mm checkpoint restore`: start the saved sessions again.
pub(crate) async fn restore_checkpoint(
    socket: &Path,
    host: Option<String>,
    dry_run: bool,
) -> Result<u8> {
    let checkpoint = Checkpoint::load()?;
    if checkpoint.is_empty() {
        println!("nothing has been checkpointed; `mm checkpoint save` writes one");
        return Ok(OK);
    }
    let wanted: Vec<&Kept> = checkpoint
        .sessions
        .iter()
        .filter(|kept| {
            host.as_deref()
                .is_none_or(|only| same_machine(only, &kept.host))
        })
        .collect();
    if let Some(only) = &host
        && wanted.is_empty()
    {
        // Narrowing to a machine the file says nothing about, which is a typo
        // far more often than it is a machine that had no sessions. Reported
        // rather than answered with "put back 0 sessions", which reads as
        // success.
        eprintln!("mm: the checkpoint has nothing on {only}");
        return Ok(FAILED);
    }

    if dry_run {
        for kept in &wanted {
            println!("{}", restoring(kept));
            if let Some(line) = spawning(kept) {
                println!("  {}", style::faint(&line));
            }
        }
        return Ok(OK);
    }

    // In file order and one at a time, because the start time is stamped at the
    // spawn: restoring them in the order they were opened in is what puts every
    // listing's rows back where they were.
    let mut started = 0;
    let mut here = Vec::new();
    let mut failed = 0;
    let mut reached = Vec::new();
    for kept in &wanted {
        let spawned = spawn_kept(socket, kept).await;
        match spawned {
            Ok(Some(name)) => {
                println!("{} {}", style::green("✓"), restoring(kept));
                if !is_this_machine(&kept.host) {
                    reached.push(kept.host.clone());
                }
                started += 1;
                here.push(Back {
                    host: kept.host.clone(),
                    name,
                    group: kept.group.clone(),
                });
            }
            // Already running, which is what a restore run twice looks like,
            // and what a machine that put its own sessions back looks like.
            //
            // Its group is still put back, and that is the whole of the
            // cross-machine story: the machine restores the sessions and the
            // client that holds the groups restores those, so the second half
            // arrives to find every name already taken. Skipping the grouping
            // here, as this first did, left that documented sequence doing
            // nothing at all and saying it had worked.
            Ok(None) => {
                println!(
                    "  {} is already running on {}",
                    style::bold(&kept.name),
                    kept.host
                );
                here.push(Back {
                    host: kept.host.clone(),
                    name: kept.name.clone(),
                    group: kept.group.clone(),
                });
            }
            Err(e) => {
                eprintln!("mm: {}: {e:#}", qualified(&kept.host, &kept.name));
                failed += 1;
            }
        }
    }

    reached.sort();
    reached.dedup();
    note_reached(socket, reached).await;

    // Only now do the new pids exist, and membership is keyed on them.
    if here.iter().any(|back| back.group.is_some()) {
        let listing = everywhere(socket).await?;
        let put_back = |groups: &mut Groups| {
            for Back { host, name, group } in &here {
                let Some(group) = group else { continue };
                // Matched the way every host name here is matched. A raw string
                // compare missed a file spelling this machine `local`, which is
                // the spelling `--host` documents and a hand-edited file is likely
                // to use, and the miss was silent: the line above had already said
                // the group was put back.
                let found = listing
                    .sessions
                    .iter()
                    .find(|h| same_machine(&h.host, host) && h.session.name == *name);
                let Some(hosted) = found else {
                    eprintln!(
                        "mm: {} came back but could not be found again, so it is \
                     not in {group}",
                        qualified(host, name)
                    );
                    failed += 1;
                    continue;
                };
                // Reported rather than propagated. The names come out of a file
                // somebody may have edited, so one that `assign` refuses is one
                // bad line: raised as an error here it would abandon the loop
                // after every session had already been started, save no grouping
                // at all, and fail a command whose sessions are all back.
                if let Err(e) = groups.assign(group, host, &hosted.session) {
                    eprintln!("mm: {}: {e:#}", qualified(host, name));
                    failed += 1;
                }
            }
            Ok(())
        };
        Groups::change(put_back)?;
    }

    let machines = here
        .iter()
        .map(|back| back.host.as_str())
        .collect::<HashSet<_>>()
        .len();
    println!(
        "{} put back {started} session{} on {machines} machine{}",
        style::green("✓"),
        if started == 1 { "" } else { "s" },
        if machines == 1 { "" } else { "s" }
    );
    Ok(if failed > 0 { FAILED } else { OK })
}

/// A saved session that is running again, whether this restore started it or
/// found it already there, kept for putting it back in its group.
struct Back {
    host: String,
    /// The name it is running under, which is the node's answer to the spawn.
    name: String,
    group: Option<String>,
}

/// Start one saved session, or answer `None` if that name is already running.
///
/// Like `N` in control mode, this sends a directory read from the destination
/// machine. `mm new` has only the caller's directory, so it sends it locally.
async fn spawn_kept(socket: &Path, kept: &Kept) -> Result<Option<String>> {
    let spec = checkpoint::spec_for(kept, attach::session_size());
    let mut stream = open_or_start(socket, &kept.host).await?;
    match stream.request(&Request::Spawn(spec)).await? {
        Response::Spawned { name } => Ok(Some(name)),
        // The node's way of saying the name is taken. Told apart by asking
        // rather than by parsing: a listing first would be a second round trip
        // per machine and still stale by the time the spawn landed.
        Response::Error(e) if e.contains("already exists") => Ok(None),
        Response::Error(e) => bail!(e),
        other => bail!("unexpected response to a spawn: {other:?}"),
    }
}

/// One line of what a restore is about to do, or would do.
fn restoring(kept: &Kept) -> String {
    let what = if kept.command.is_empty() {
        "a login shell".to_string()
    } else {
        kept.command.join(" ")
    };
    let group = kept
        .group
        .as_ref()
        .map(|g| format!(" {}", style::faint(&format!("@{g}"))))
        .unwrap_or_default();
    format!(
        "{}{group} {} {}",
        style::bold(&qualified(&kept.host, &kept.name)),
        what,
        style::faint(&format!("in {}", kept.cwd))
    )
}

/// The line the node will be handed for one entry, spelled the way it will
/// reach a shell.
///
/// Worth a row of its own because the row above it is not the whole story: what
/// runs is the captured command inside the wrapper that keeps the login shell
/// behind it (`checkpoint::to_spawn`), and the one question anybody asks a
/// checkpoint before restarting a machine is what it is actually going to run.
/// Built from the same `to_spawn` the restore sends and joined with the same
/// quoter the node runs it through (`shell::join`), so this cannot become a
/// second opinion about what happens.
///
/// `None` for a session that was sitting at a prompt: an empty spawn is the
/// login shell, which the row above already says in words.
fn spawning(kept: &Kept) -> Option<String> {
    let argv = checkpoint::to_spawn(&kept.command);
    (!argv.is_empty()).then(|| shell::join(&argv))
}

/// `mm checkpoint show`: what is written down, and nothing else.
///
/// Contacts no machine on purpose. This is the one command that still answers
/// when everything is being restarted, which is exactly when somebody wants to
/// know what they are about to get back.
pub(crate) fn show_checkpoint() -> Result<u8> {
    let checkpoint = Checkpoint::load()?;
    if checkpoint.is_empty() {
        println!("nothing has been checkpointed; `mm checkpoint save` writes one");
        return Ok(OK);
    }
    println!(
        "{}",
        style::faint(&format!(
            "{} sessions, saved {} ago",
            checkpoint.sessions.len(),
            term::duration(Checkpoint::now().saturating_sub(checkpoint.taken))
        ))
    );
    for kept in &checkpoint.sessions {
        println!("{}", restoring(kept));
        if let Some(line) = spawning(kept) {
            println!("  {}", style::faint(&line));
        }
    }
    Ok(OK)
}
