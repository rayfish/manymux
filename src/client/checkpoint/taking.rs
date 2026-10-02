//! What a save decides to write down, worked out away from the asking and the
//! printing.
//!
//! By the time this runs the questions have all been put: every machine has
//! been listed, and every machine that answered has been asked what its
//! sessions are doing. What is left is a judgement on each session, and every
//! part of it has gone wrong at least once in a way that was only noticed much
//! later, with a restart already behind it:
//!
//! - A session nothing can describe is left out rather than written down half
//!   way, because an entry with the wrong directory in it puts somebody's work
//!   back in `$HOME`.
//! - A session that is left out keeps whatever an earlier save knew about it.
//!   Dropping it instead is how a good checkpoint taken at ten o'clock was
//!   destroyed by one at five past that happened to catch a build mid-pipeline.
//! - A machine this save did not ask about keeps its entries too, or updating
//!   one machine would throw away the record covering the other three.
//!
//! So the counts matter as much as the file: `--keep-sessions` weighs
//! [`Taken::lost`] against what is running and refuses the restart rather than
//! proceed on a record with holes in it. The words all of this is reported in
//! belong to the binary; what is decided is here, where it can be tested
//! without a fleet.

use std::collections::{HashMap, HashSet};

use crate::client::checkpoint::{Kept, resumed, resumes_by_directory, still_wrapped};
use crate::client::groups::Groups;
use crate::client::switch::Located;
use crate::hosts::{is_this_machine, same_machine};
use crate::proto::{Doing, HostedSession};

/// Everything a save has to weigh up, once the asking is done.
pub struct Taking<'a> {
    /// Every session every machine answered the listing with.
    pub sessions: &'a [HostedSession],
    /// The machine `--host` narrowed to, in the spelling it was typed in.
    pub only: Option<&'a str>,
    /// The machines that answered the listing.
    pub answering: &'a [String],
    /// The machines that answered the listing and then would not say what their
    /// sessions were doing. Their sessions are counted as lost, but no word is
    /// said about each one: the machine is reported once instead.
    pub refused: &'a [String],
    /// What each session is doing, by where it is.
    pub doing: &'a HashMap<Located, Doing>,
    /// Which session is in which group, which only this end knows.
    pub groups: &'a Groups,
    /// The session this command is running in, if it is running in one.
    pub ours: Option<u32>,
    /// What the file says already. Entries this save has nothing to put in the
    /// place of are carried over from it.
    pub before: Vec<Kept>,
}

/// Why a session that is running is not in the file.
///
/// Reported by the caller, which has the host names and the words. Kept as
/// reasons rather than sentences so that a test can say which rule fired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsaid {
    /// Its machine answered the listing and then said nothing about this
    /// session, which is not the same as refusing the question.
    NothingSaid,
    /// Nothing can say where it is, and a guess would be `$HOME`.
    NoDirectory,
    /// Nothing can say what it is running. Its leader has been reaped while the
    /// rest of a pipeline runs on, or it is a zombie.
    NoCommand,
    /// It ended and another session took its name between the two questions, so
    /// what was learned is about one session and the name is another's.
    Changed,
    /// Still inside a wrapper an earlier restore put it in, which nothing here
    /// can read back through. A moment later it will be readable.
    StillStarting,
}

/// One session that is running and is not in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub at: Located,
    pub why: Unsaid,
}

/// Two sessions that would come back on the same conversation.
///
/// `claude --continue` and `pi --continue` pick up the newest session in the
/// directory they are run in, which is the whole of what makes a checkpoint
/// work without capturing an id. Two sessions on one machine in one directory
/// therefore resume the same one, and the second to start loses. Nothing here
/// can tell them apart: neither program leaves its transcript open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shared {
    /// The earlier of the two in listing order, which is the order they are
    /// written down and started again in. So it is the one that loses: what a
    /// restore leaves behind is whichever came back last.
    pub first: String,
    pub second: String,
    pub cwd: String,
}

/// What a save came to: the file to write, and everything worth saying about
/// how it got there.
#[derive(Debug, Default)]
pub struct Taken {
    /// What to write. The sessions this save described come first, in listing
    /// order, and the entries carried over from the last one follow.
    pub sessions: Vec<Kept>,
    /// Sessions that are running and could not be written down, in listing
    /// order.
    pub dropped: Vec<Dropped>,
    /// Pairs that resume the same conversation. Worth saying at the save rather
    /// than at the restore: the file is editable, and this is the moment
    /// somebody can name one of them by hand.
    pub shared: Vec<Shared>,
    /// Entries written for the machines this save asked about.
    pub written: usize,
    /// How many machines those are.
    pub machines: usize,
    /// Entries kept from an earlier save because this one did not ask their
    /// machine.
    pub carried: usize,
    /// Entries kept from an earlier save because this one could not describe
    /// the session, which is a record older than this moment.
    pub kept_from_before: usize,
    /// Sessions that are running and are not in the file: the ones that could
    /// not be described, and every session on a machine that refused. What
    /// `--keep-sessions` has to be told before it authorises a restart.
    pub lost: usize,
}

impl Taking<'_> {
    /// Whether this save is about the machine called `at`.
    fn wanted(&self, at: &str) -> bool {
        self.only.is_none_or(|only| same_machine(only, at))
    }
}

/// Work out what to write.
///
/// The order is the listing's, which is by machine and then oldest first, and
/// it is the order every screen shows sessions in.
pub fn take(taking: Taking<'_>) -> Taken {
    let mut taken = Taken::default();
    // Sessions that are running on a machine that answered and are not being
    // written down. Their earlier entry is kept rather than dropped, which is
    // what tells a later restore they are still out there.
    let mut undescribed: HashSet<Located> = HashSet::new();

    for hosted in taking.sessions {
        if !taking.wanted(&hosted.host) {
            continue;
        }
        let at = Located::new(&hosted.host, &hosted.session.name);
        let mut give_up = |why: Unsaid, taken: &mut Taken| {
            taken.dropped.push(Dropped {
                at: at.clone(),
                why,
            });
            undescribed.insert(at.clone());
            taken.lost += 1;
        };
        let Some(found) = taking.doing.get(&at) else {
            // Its machine refused the question, and is reported once rather
            // than once per session on it.
            if !taking.refused.contains(&hosted.host) {
                give_up(Unsaid::NothingSaid, &mut taken);
            }
            continue;
        };
        let Some(cwd) = found.cwd.clone() else {
            give_up(Unsaid::NoDirectory, &mut taken);
            continue;
        };
        // The session this command was typed in. Its work is this command, and
        // it will be a prompt in the right place again the moment this returns.
        let ours_this_one =
            is_this_machine(&hosted.host) && taking.ours == Some(hosted.session.pid);
        // Nothing known is never a prompt: a session sitting at one has its
        // shell in front of it, so the argv holds the shell. Empty means the
        // read found no process to describe. Recorded as a prompt, as this once
        // did, a session running a build comes back as an empty shell and
        // nothing anywhere says so.
        if found.foreground.is_empty() && !ours_this_one {
            give_up(Unsaid::NoCommand, &mut taken);
            continue;
        }
        // The pid the machine answered with, against the one the listing gave.
        // A session that ended and was replaced by another of the same name
        // between the two questions is one this would otherwise write down with
        // the wrong work against the right name.
        if found.pid != hosted.session.pid {
            give_up(Unsaid::Changed, &mut taken);
            continue;
        }
        let command = if ours_this_one {
            Vec::new()
        } else {
            resumed(&found.foreground)
        };
        // A wrapper from an earlier restore that nothing above could read
        // through. Asked of the answer rather than of the raw foreground, which
        // is the whole distinction: the plain wrapper shape *is* read through,
        // and it is what a machine reports in the moment between the spawn and
        // the command reaching the front of the terminal. Refusing on the raw
        // form failed a save taken straight after a restore, on a runner slow
        // enough for that moment to be the one being asked about. What is left
        // here is the shape nothing can read: quoted inside a login shell's own
        // snippet, which would gain another shell on every save and restore.
        if still_wrapped(&command) {
            give_up(Unsaid::StillStarting, &mut taken);
            continue;
        }
        taken.sessions.push(Kept {
            host: hosted.host.clone(),
            name: hosted.session.name.clone(),
            cwd,
            group: taking
                .groups
                .group_of(&hosted.host, &hosted.session)
                .map(str::to_string),
            command,
        });
    }

    taken.shared = shared_directories(&taken.sessions);
    taken.machines = taken
        .sessions
        .iter()
        .map(|kept| kept.host.as_str())
        .collect::<HashSet<_>>()
        .len();
    taken.written = taken.sessions.len();

    // Only what this save actually learned about is replaced. A machine that
    // was not asked (`--host` elsewhere, and `--keep-sessions` always narrows
    // to this one) or that refused the question has said nothing, and reading
    // that silence as "it has no sessions" would throw away a checkpoint taken
    // at somebody's desk covering three machines the moment they updated one of
    // them. The same argument `Groups::prune` makes about a machine that is
    // asleep, for the same reason.
    let answered: Vec<&String> = taking
        .answering
        .iter()
        .filter(|at| taking.wanted(at) && !taking.refused.iter().any(|host| host == *at))
        .collect();
    let mut before = taking.before;
    // Compared the way every other host name is compared, and not as strings. A
    // file naming this machine `local` (a spelling a restore accepts and a
    // hand-edited file is likely to use) would otherwise miss the `devbox` in
    // this list, be carried over, and be written again beside the fresh entry:
    // one session listed twice, the stale copy holding a stale directory. The
    // same happens on its own to a machine whose short hostname changed.
    //
    // A session that could not be described this time keeps its earlier entry
    // if there is one. Its host answered, so the rule above would drop it, and
    // dropping it is how a good checkpoint taken at ten o'clock was destroyed
    // by a save at five past that happened to catch a session mid-pipeline: the
    // record went, and the restart it was taken for was refused in the same
    // breath, leaving neither. The entry is older than this moment and is said
    // to be, and the session is still counted against the save, so nothing acts
    // on it without somebody deciding to.
    before.retain(|kept| {
        if undescribed.contains(&Located::new(&kept.host, &kept.name)) {
            taken.kept_from_before += 1;
            return true;
        }
        !answered.iter().any(|at| same_machine(at, &kept.host))
    });
    taken.carried = before.len() - taken.kept_from_before;
    taken.sessions.extend(before);

    // Every session on a machine that refused the question is lost too, and the
    // machine is what gets reported rather than each of them.
    for host in taking.refused {
        taken.lost += taking
            .sessions
            .iter()
            .filter(|hosted| hosted.host == *host)
            .count();
    }

    taken
}

/// Pairs that would resume the same conversation, in the order they were
/// written down. See [`Shared`].
fn shared_directories(sessions: &[Kept]) -> Vec<Shared> {
    let mut shared = Vec::new();
    let mut seen: HashMap<(&str, &str), &Kept> = HashMap::new();
    for kept in sessions {
        if !resumes_by_directory(&kept.command) {
            continue;
        }
        let key = (kept.host.as_str(), kept.cwd.as_str());
        match seen.get(&key) {
            Some(first) => shared.push(Shared {
                first: first.name.clone(),
                second: kept.name.clone(),
                cwd: kept.cwd.clone(),
            }),
            None => {
                seen.insert(key, kept);
            }
        }
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::checkpoint::{KEEP_THE_SHELL, to_spawn};
    use crate::hosts::{LOCAL, this_machine};
    use crate::proto::{SessionInfo, Size};
    use std::time::UNIX_EPOCH;

    fn hosted(host: &str, name: &str, pid: u32) -> HostedSession {
        HostedSession {
            host: host.to_string(),
            session: SessionInfo {
                name: name.to_string(),
                title: name.to_string(),
                command: "zsh".into(),
                pid,
                size: Size::new(80, 24),
                attached: 0,
                idle: 0,
                bells: 0,
                started: UNIX_EPOCH,
                node: String::new(),
            },
        }
    }

    fn doing(name: &str, pid: u32, cwd: Option<&str>, foreground: &[&str]) -> Doing {
        Doing {
            name: name.to_string(),
            pid,
            cwd: cwd.map(str::to_string),
            foreground: foreground.iter().map(|w| w.to_string()).collect(),
        }
    }

    fn answers(doing: Vec<(&str, Doing)>) -> HashMap<Located, Doing> {
        doing
            .into_iter()
            .map(|(host, d)| (Located::new(host, &d.name), d))
            .collect()
    }

    fn kept(host: &str, name: &str, cwd: &str) -> Kept {
        Kept {
            host: host.to_string(),
            name: name.to_string(),
            cwd: cwd.to_string(),
            group: None,
            command: vec!["cargo".into(), "build".into()],
        }
    }

    /// Everything but the sessions and the file, which is what each test varies.
    fn taking<'a>(
        sessions: &'a [HostedSession],
        answering: &'a [String],
        doing: &'a HashMap<Located, Doing>,
        groups: &'a Groups,
    ) -> Taking<'a> {
        Taking {
            sessions,
            only: None,
            answering,
            refused: &[],
            doing,
            groups,
            ours: None,
            before: Vec::new(),
        }
    }

    /// A directory nothing can name is the one thing an entry cannot do without:
    /// written down anyway, the program comes back in `$HOME` and picks up
    /// somebody else's work.
    #[test]
    fn a_session_nothing_can_place_or_describe_is_left_out() {
        let sessions = vec![hosted("box", "nowhere", 1), hosted("box", "silent", 2)];
        let answering = vec!["box".to_string()];
        let doing = answers(vec![
            ("box", doing("nowhere", 1, None, &["cargo", "build"])),
            ("box", doing("silent", 2, Some("/srv/project"), &[])),
        ]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert_eq!(taken.written, 0, "neither could be described");
        assert_eq!(taken.lost, 2);
        assert_eq!(
            taken
                .dropped
                .iter()
                .map(|d| (d.at.session.as_str(), d.why))
                .collect::<Vec<_>>(),
            vec![
                ("nowhere", Unsaid::NoDirectory),
                ("silent", Unsaid::NoCommand),
            ]
        );
    }

    /// A session that ended and was replaced by another of the same name between
    /// the two questions: what was learned is about one session and the name is
    /// another's, so the pair is no use to anybody.
    #[test]
    fn a_session_that_changed_under_the_question_is_left_out() {
        let sessions = vec![hosted("box", "build", 7)];
        let answering = vec!["box".to_string()];
        let doing = answers(vec![(
            "box",
            doing("build", 99, Some("/srv/project"), &["cargo", "build"]),
        )]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert_eq!(taken.written, 0);
        assert_eq!(taken.dropped.first().map(|d| d.why), Some(Unsaid::Changed));
    }

    /// A session caught in the moment between a restore's spawn and the command
    /// reaching the front of its terminal. Written down, the wrapper would gain
    /// another shell on every save and restore.
    #[test]
    fn a_session_still_inside_a_restores_wrapper_is_left_out() {
        let sessions = vec![hosted("box", "claude", 3)];
        let answering = vec!["box".to_string()];
        let script = format!("'claude' {KEEP_THE_SHELL}");
        let doing = answers(vec![(
            "box",
            doing("claude", 3, Some("/srv/project"), &["claude", &script]),
        )]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert_eq!(taken.written, 0);
        assert_eq!(
            taken.dropped.first().map(|d| d.why),
            Some(Unsaid::StillStarting),
            "a wrapper nothing can read through was written down"
        );
    }

    /// The wrapper a restore leaves in front of a session is read back through
    /// rather than refused, and that is why `still_wrapped` is asked of the
    /// resumed command instead of the raw foreground: the plain wrapper shape is
    /// exactly what a machine reports in the moment between a spawn and the
    /// command reaching the front of its terminal. Asked of the raw form, this
    /// failed a save taken straight after a restore, on a runner slow enough for
    /// that moment to be the one being asked about.
    #[test]
    fn a_session_a_restore_wrapped_is_read_back_through_rather_than_left_out() {
        let sessions = vec![hosted("box", "claude", 5)];
        let answering = vec!["box".to_string()];
        // What the node was handed, less the `exec` that is the shell's word
        // rather than the session's: the rest is the argv a machine answers
        // with while the wrapper is still in front.
        let ran = to_spawn(&["claude".to_string(), "--continue".to_string()]);
        let front: Vec<String> = ran[1..].to_vec();
        assert!(
            still_wrapped(&front),
            "the argv handed over has to carry the marker for this to prove \
             anything: {front:?}"
        );
        let front: Vec<&str> = front.iter().map(String::as_str).collect();
        let doing = answers(vec![(
            "box",
            doing("claude", 5, Some("/srv/project"), &front),
        )]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert!(taken.dropped.is_empty(), "{:?}", taken.dropped);
        assert_eq!(taken.written, 1);
        assert_eq!(
            taken.sessions[0].command,
            vec!["claude".to_string(), "--continue".to_string()],
            "the wrapper was written down instead of what it wraps"
        );
    }

    /// `ours` is a pid on *this* machine. Another machine's session that happens
    /// to wear the same number is not this command, and read as one it would be
    /// written down as a prompt with whatever it was running thrown away.
    #[test]
    fn another_machines_session_wearing_our_pid_is_not_this_command() {
        // A name no machine answers to, since the whole question is whether it
        // is read as this one.
        let elsewhere = "not-this-machine";
        assert!(!is_this_machine(elsewhere));
        let sessions = vec![hosted(elsewhere, "work", 42)];
        let answering = vec![elsewhere.to_string()];
        let doing = answers(vec![(
            elsewhere,
            doing("work", 42, Some("/srv/project"), &[]),
        )]);
        let groups = Groups::default();
        let taken = take(Taking {
            ours: Some(42),
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert_eq!(taken.written, 0, "it was taken for this command");
        assert_eq!(
            taken.dropped.first().map(|d| d.why),
            Some(Unsaid::NoCommand)
        );
    }

    /// A machine that refused the question is reported once, rather than once
    /// per session on it. Its sessions still count against the save.
    #[test]
    fn a_machine_that_refused_the_question_is_not_reported_per_session() {
        let sessions = vec![hosted("box", "one", 1), hosted("box", "two", 2)];
        let answering = vec!["box".to_string()];
        let doing = HashMap::new();
        let refused = vec!["box".to_string()];
        let groups = Groups::default();
        let taken = take(Taking {
            refused: &refused,
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert!(taken.dropped.is_empty(), "{:?}", taken.dropped);
        assert_eq!(taken.lost, 2, "both are running and neither is in the file");
    }

    /// The failure this closes: a good checkpoint taken at ten o'clock was
    /// destroyed by one at five past that caught a session mid-pipeline. The
    /// record went, and the restart it was taken for was refused in the same
    /// breath, leaving neither.
    #[test]
    fn a_session_this_save_could_not_describe_keeps_what_the_last_one_knew() {
        let sessions = vec![hosted("box", "build", 1)];
        let answering = vec!["box".to_string()];
        let doing = answers(vec![("box", doing("build", 1, None, &["cargo", "build"]))]);
        let groups = Groups::default();
        let taken = take(Taking {
            before: vec![kept("box", "build", "/srv/project")],
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert_eq!(taken.kept_from_before, 1);
        assert_eq!(taken.carried, 0, "its machine was asked, so it is not that");
        assert_eq!(
            taken.sessions.first().map(|k| k.cwd.as_str()),
            Some("/srv/project"),
            "the earlier entry was dropped instead of kept"
        );
        assert_eq!(taken.lost, 1, "it is still running and still not described");
    }

    /// Updating one machine must not throw away the record covering the other
    /// three. A machine this save did not ask has said nothing, and silence is
    /// not "it has no sessions".
    #[test]
    fn a_machine_this_save_did_not_ask_about_keeps_its_entries() {
        let sessions = vec![hosted("box", "build", 1), hosted("gpu", "train", 2)];
        let answering = vec!["box".to_string(), "gpu".to_string()];
        let doing = answers(vec![
            (
                "box",
                doing("build", 1, Some("/srv/project"), &["cargo", "build"]),
            ),
            (
                "gpu",
                doing("train", 2, Some("/srv/model"), &["python", "train.py"]),
            ),
        ]);
        let groups = Groups::default();
        let taken = take(Taking {
            only: Some("box"),
            before: vec![kept("gpu", "train", "/srv/model")],
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert_eq!(taken.written, 1, "only box was asked about");
        assert_eq!(taken.machines, 1);
        assert_eq!(taken.carried, 1, "gpu's entry was thrown away");
        assert_eq!(taken.lost, 0, "gpu was never asked, so nothing is lost");
        assert_eq!(taken.sessions.len(), 2);
    }

    /// A file naming this machine `local` is a spelling `--host` documents and a
    /// hand-edited file is likely to use. Compared as strings it missed the
    /// machine's own name, was carried over, and was written again beside the
    /// fresh entry: one session listed twice, the stale copy holding a stale
    /// directory.
    #[test]
    fn an_entry_spelling_this_machine_local_is_replaced_rather_than_carried() {
        let here = this_machine();
        let sessions = vec![hosted(here, "build", 1)];
        let answering = vec![here.to_string()];
        let doing = answers(vec![(
            here,
            doing("build", 1, Some("/srv/project"), &["cargo", "build"]),
        )]);
        let groups = Groups::default();
        let taken = take(Taking {
            before: vec![kept(LOCAL, "build", "/somewhere/it/used/to/be")],
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert_eq!(taken.carried, 0);
        assert_eq!(taken.sessions.len(), 1, "{:?}", taken.sessions);
        assert_eq!(taken.sessions[0].cwd, "/srv/project");
    }

    /// Both come back on the same conversation and the newest wins, which is
    /// worth saying while the file can still be edited by hand.
    #[test]
    fn two_sessions_resuming_in_one_directory_are_reported() {
        let sessions = vec![hosted("box", "claude", 1), hosted("box", "claude-2", 2)];
        let answering = vec!["box".to_string()];
        let doing = answers(vec![
            ("box", doing("claude", 1, Some("/srv/project"), &["claude"])),
            (
                "box",
                doing("claude-2", 2, Some("/srv/project"), &["claude"]),
            ),
        ]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert_eq!(taken.written, 2, "both are written down either way");
        assert_eq!(
            taken.shared,
            vec![Shared {
                first: "claude".to_string(),
                second: "claude-2".to_string(),
                cwd: "/srv/project".to_string(),
            }]
        );
    }

    /// Two sessions in one directory where neither resumes by directory is
    /// nothing worth saying: `cargo build` twice is two builds.
    #[test]
    fn two_sessions_in_one_directory_that_resume_nothing_are_not_reported() {
        let sessions = vec![hosted("box", "one", 1), hosted("box", "two", 2)];
        let answering = vec!["box".to_string()];
        let doing = answers(vec![
            (
                "box",
                doing("one", 1, Some("/srv/project"), &["cargo", "build"]),
            ),
            (
                "box",
                doing("two", 2, Some("/srv/project"), &["cargo", "test"]),
            ),
        ]);
        let taken = take(taking(&sessions, &answering, &doing, &Groups::default()));

        assert!(taken.shared.is_empty(), "{:?}", taken.shared);
    }

    /// The session this command was typed in. Recorded as it stands it would
    /// come back as `mm checkpoint save`, and a restore would rewrite the file
    /// it is walking; it is a prompt again the moment the command returns.
    #[test]
    fn the_session_this_command_runs_in_comes_back_as_a_prompt() {
        let here = this_machine();
        let sessions = vec![hosted(here, "work", 42)];
        let answering = vec![here.to_string()];
        // Nothing in front of it: on every other session that is a read that
        // found nothing, and is left out.
        let doing = answers(vec![(here, doing("work", 42, Some("/srv/project"), &[]))]);
        let groups = Groups::default();
        let taken = take(Taking {
            ours: Some(42),
            ..taking(&sessions, &answering, &doing, &groups)
        });

        assert!(taken.dropped.is_empty(), "{:?}", taken.dropped);
        assert_eq!(taken.written, 1);
        assert!(
            taken.sessions[0].command.is_empty(),
            "it came back running {:?}",
            taken.sessions[0].command
        );
    }
}
