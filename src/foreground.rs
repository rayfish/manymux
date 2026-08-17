//! What a session is working on, as opposed to what it was started with.
//!
//! The node starts a session and remembers the argv it used, and that is the
//! last thing it knows about it: somebody then spends three hours running
//! things inside that shell and moving around the filesystem, and none of it
//! reaches any structure here. A checkpoint built from the spawn would put
//! every session back at a prompt in the home directory, which is the one
//! place nobody was.
//!
//! So it is read from the operating system at the moment it is asked for, and
//! the thing to ask about is the terminal's *foreground process group*: what
//! the shell puts a command into while it runs and takes back when it
//! finishes. That is exactly "what is running in this session right now".
//!
//! It lives in the library rather than under `node` because both ends need it,
//! and for a reason worth keeping: a client on the same machine as a session
//! can read this itself, from the pid the listing already carries. That is the
//! only way the first checkpoint on any machine can be taken at all, since the
//! node holding those sessions is by definition running the build from before
//! this existed and will refuse to be asked. See `client::checkpoint`.
//!
//! Which is why the *parsing* here is split from the *reading* and none of it
//! is gated to Linux: [`Raw`] applies every rule below to bytes somebody else
//! fetched, so a machine reached over ssh is read by the same code, and a
//! checkpoint of a Linux box can be taken from a Mac.
//!
//! Reading is Linux only, and answers a blank rather than a guess anywhere
//! else. macOS has no `/proc`: the directory would come from `libproc` and the
//! argv from a `KERN_PROCARGS2` sysctl, which is a separate piece of work. A
//! blank is a real answer here, and a much better one than a wrong directory:
//! a session restored in the wrong place resumes somebody else's conversation.
//! Note that this cuts only one way — a Mac cannot describe its *own*
//! sessions, but it can describe a Linux machine's over ssh.

/// The program holding a session's terminal, and the directory it is in.
///
/// Both halves are best-effort and neither is an error. A process that exits
/// between two reads of `/proc` is ordinary rather than exceptional, and one
/// session that cannot be described must not cost the other eight theirs.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Foreground {
    /// Where that program is. `None` where the machine cannot say.
    pub cwd: Option<String>,
    /// Its argv, which for a session sitting at a prompt is the shell itself.
    /// Empty means nothing is known, which is *not* the same as a prompt and
    /// is not treated as one anywhere: a session at a prompt has its shell in
    /// front of it and so has an argv. It comes up when the foreground group's
    /// leader has been reaped while the rest of a pipeline runs on, and when
    /// the process is a zombie, whose `cmdline` reads empty.
    pub argv: Vec<String>,
}

/// What the session led by `leader` is working on.
///
/// `leader` is the process the node spawned, which owns the PTY slave. It is
/// both the thing to ask which process group is in front and the fallback for
/// whatever the answer cannot supply.
///
/// Note that the leader is very often the answer to its own question. A shell
/// `exec`s a simple final command, so a session started as `mm new box claude`
/// *is* claude, with the same pid the node recorded. Nothing here may read
/// that equality as "no command is running": it is the ordinary case, and the
/// argv is what tells a prompt from a program.
#[cfg(target_os = "linux")]
pub fn of(leader: u32) -> Foreground {
    // Both halves come from one process, or neither does. The foreground
    // process is where the work is and is free to have moved: a `cd` in a
    // subshell, a build started from somewhere else. Falling back to the
    // *leader's* directory when only the front's could not be read, as this
    // first did, pairs one process's directory with another's command and
    // marks the answer no differently: [`cwd_of`] refuses a deleted directory
    // precisely so that no wrong one is offered, and the fallback handed one
    // back a line later. A session whose build had `cd`ed into a directory
    // since removed came out recorded in the leader's, which for a program
    // that resumes by directory is somebody else's conversation.
    let front = in_front_of(leader);
    Foreground {
        cwd: cwd_of(front),
        argv: argv_of(front),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn of(_leader: u32) -> Foreground {
    Foreground::default()
}

/// The session this process is itself running in, as a session leader's pid.
///
/// A terminal session in the POSIX sense is what the node gives each session
/// it spawns, so the id here *is* the pid the node recorded, for anything
/// running inside one. Compared against a listing, that says which of a
/// machine's sessions this command was typed in, and it is the only thing that
/// says it reliably: `MM_SESSION` is stamped into the environment once at spawn
/// and a rename cannot reach back into it, so a session renamed since is a
/// session this would fail to recognise.
///
/// `None` where there is nothing to compare, which is any process not in a
/// session of the node's.
pub fn our_session() -> Option<u32> {
    // Safe: `getsid` reads process state and touches nothing of ours.
    let sid = unsafe { libc::getsid(0) };
    (sid > 0).then_some(sid as u32)
}

/// The `tpgid` field of a `/proc/<pid>/stat` line.
///
/// Read from after the *last* `)` rather than by splitting the line from the
/// left, because the second field is the executable's name in parentheses and
/// an executable is free to be called `foo (old) (2)`. Counting columns from
/// the left walks into the middle of that name and reads a different field,
/// for those processes only, which is the kind of bug that works on every
/// machine it is tested on.
///
/// Not gated to Linux, and none of the three below are: a client reading a
/// machine it reached over ssh parses the same bytes on whatever it happens to
/// be running, which is how a checkpoint of a Linux box can be taken from a
/// Mac. Only the *reading* is Linux's.
pub fn tpgid(stat: &str) -> Option<i32> {
    let after = &stat[stat.rfind(')')? + 1..];
    // What follows the name is state, ppid, pgrp, session, tty_nr, tpgid.
    after.split_whitespace().nth(5)?.parse().ok()
}

/// Which process a `tpgid` names, given the leader it was read from.
///
/// Both halves of an answer come from one process or neither does, so this is
/// where that is decided: a `tpgid` that is not usable, or that is the leader
/// itself, means the leader is what to describe.
pub fn in_front(leader: u32, tpgid: Option<i32>) -> u32 {
    match tpgid {
        Some(front) if front > 0 && front as u32 != leader => front as u32,
        _ => leader,
    }
}

/// Where a process is, given what its `cwd` link points at.
///
/// A directory that has been deleted out from under the process reads as
/// `<path> (deleted)`, which is the kernel annotating rather than answering.
/// Restoring into a directory of that literal name is not what anybody meant,
/// so it is refused: no directory beats a wrong one everywhere here.
pub fn cwd_from(link: &str) -> Option<String> {
    (!link.is_empty() && !link.ends_with(" (deleted)")).then(|| link.to_string())
}

#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    // Not lossy: a path that is not UTF-8 cannot be put in a `SpawnSpec`, and
    // a lossily mangled one names a directory that does not exist.
    cwd_from(&path.into_os_string().into_string().ok()?)
}

/// The argv of a process, as separate words, or nothing if it cannot be read
/// back exactly.
///
/// Only the *trailing* empty words are dropped, and that is not tidying. A
/// program that rewrites its own process title writes over the whole argv
/// block and pads the rest of it with NULs: `pi` does, so read literally its
/// argv is `pi` and seventy-five empty arguments, each of which a restore
/// would quote into `''` and hand to the program as something it never
/// received. Every `cmdline` ends in a NUL besides. An empty word *between*
/// two others is a real argument and is kept: dropping those, as this first
/// did, turns `git commit --allow-empty-message -m ''` into a command missing
/// its message.
///
/// Not UTF-8 is nothing rather than something close to it. `String::from_utf8_lossy`
/// would put a replacement character where a byte was, and the caller would go
/// on to quote that and exec it: a latin-1 filename in an argument comes back
/// as a different command with nothing said. The same argument [`cwd_of`]
/// makes about a path it cannot represent, and it has to be the same answer,
/// since a blank here is reported and counted while a mangled word is not.
pub fn argv_from(raw: &[u8]) -> Vec<String> {
    let Ok(text) = std::str::from_utf8(raw) else {
        return Vec::new();
    };
    let mut argv: Vec<String> = text.split('\0').map(str::to_string).collect();
    while argv.last().is_some_and(|last| last.is_empty()) {
        argv.pop();
    }
    argv
}

#[cfg(target_os = "linux")]
fn argv_of(pid: u32) -> Vec<String> {
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(raw) => argv_from(&raw),
        Err(_) => Vec::new(),
    }
}

#[cfg(target_os = "linux")]
fn in_front_of(leader: u32) -> u32 {
    let stat = std::fs::read_to_string(format!("/proc/{leader}/stat")).ok();
    in_front(leader, stat.as_deref().and_then(tpgid))
}

/// The parsers above, over bytes somebody else read.
///
/// What makes a checkpoint of a machine reached over ssh possible without that
/// machine having heard of checkpoints: the far end is asked for the same
/// three files this reads locally, and every rule about what they mean is
/// applied here, once. A shell script that decided any of it would be a second
/// implementation of rules that took three review rounds to get right.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Raw {
    /// The leader's `/proc/<pid>/stat`, for the foreground process group.
    pub stat: String,
    /// What `/proc/<front>/cwd` points at.
    pub cwd: String,
    /// `/proc/<front>/cmdline`, NULs and all.
    pub cmdline: Vec<u8>,
}

impl Raw {
    pub fn read(&self) -> Foreground {
        Foreground {
            cwd: cwd_from(&self.cwd),
            argv: argv_from(&self.cmdline),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Why the field is read from the right of the name rather than by
    /// counting columns from the left. The name is arbitrary text in brackets,
    /// so a program with a space or a bracket in its name is a different line
    /// shape, and reading those wrong would misreport exactly the processes
    /// nobody has on the machine they tested on.
    #[test]
    fn the_foreground_group_is_read_past_the_last_bracket_of_the_name() {
        // The real shape: pid, (comm), state, ppid, pgrp, session, tty_nr,
        // tpgid, and a long tail nothing here reads.
        let plain = "1234 (zsh) S 1 1234 1234 34816 5678 4194304 1234 0 0";
        assert_eq!(tpgid(plain), Some(5678));

        let awkward = "1234 (my prog (old)) S 1 1234 1234 34816 5678 4194304 1 0";
        assert_eq!(
            tpgid(awkward),
            Some(5678),
            "the name is arbitrary text and the fields are the ones after it"
        );
    }

    /// Nothing usable is `None` rather than a panic or a guess: a stat file
    /// that has stopped existing is a session that has just ended.
    #[test]
    fn a_stat_line_that_says_nothing_usable_is_left_alone() {
        assert_eq!(tpgid("nonsense with no brackets"), None);
        assert_eq!(tpgid(""), None);
        assert_eq!(tpgid("1234 (zsh) S 1 1234"), None, "the line stops short");
        assert_eq!(
            tpgid("1234 (zsh) S 1 1234 1234 0 -1 4194304"),
            Some(-1),
            "read, and refused a layer up rather than here"
        );
    }

    /// Both halves of an answer come from one process, and this is the rule
    /// that decides which. Reading the pieces apart from the files means it
    /// can be asked directly, including with the bytes a far machine sent.
    #[test]
    fn which_process_to_describe_is_the_front_one_or_the_leader() {
        assert_eq!(in_front(100, Some(200)), 200, "something else is in front");
        assert_eq!(in_front(100, Some(100)), 100, "the leader itself");
        assert_eq!(in_front(100, Some(-1)), 100, "no controlling terminal");
        assert_eq!(in_front(100, Some(0)), 100, "no foreground group");
        assert_eq!(in_front(100, None), 100, "nothing could be read");
    }

    /// The same rules, over bytes somebody else fetched, which is how a machine
    /// reached over ssh is described. A drift between this and the reading path
    /// is a checkpoint that means one thing here and another there.
    #[test]
    fn the_rules_apply_to_bytes_from_anywhere() {
        let raw = Raw {
            stat: "1234 (zsh) S 1 1234 1234 34816 5678 0".to_string(),
            cwd: "/srv/project".to_string(),
            cmdline: b"claude\0--continue\0".to_vec(),
        };
        assert_eq!(tpgid(&raw.stat), Some(5678));
        assert_eq!(
            raw.read(),
            Foreground {
                cwd: Some("/srv/project".to_string()),
                argv: vec!["claude".to_string(), "--continue".to_string()],
            }
        );

        // And every refusal survives the trip: a deleted directory, an argv
        // that is not UTF-8, the padding a rewritten process title leaves.
        assert_eq!(cwd_from("/gone (deleted)"), None);
        assert_eq!(cwd_from(""), None);
        assert!(argv_from(b"caf\xe9\0").is_empty(), "not UTF-8, so nothing");
        assert_eq!(argv_from(b"pi\0\0\0\0"), vec!["pi".to_string()]);
        assert_eq!(
            argv_from(b"git\0-m\0\0next\0"),
            vec![
                "git".to_string(),
                "-m".to_string(),
                String::new(),
                "next".to_string()
            ],
            "an empty word between two others is a real argument"
        );
    }

    /// The equality that is not a signal. A shell `exec`s its final command,
    /// so a session started with one has the command in the foreground under
    /// the pid the node recorded. An earlier draft read that as "nothing is
    /// running" and threw the command away on every such session.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_session_whose_command_replaced_its_shell_is_still_running_that_command() {
        let me = std::process::id();
        // The test binary is its own process group leader in the usual case;
        // whatever the answer, asking about ourselves gives our own argv back
        // rather than an empty one.
        let front = in_front_of(me);
        assert!(
            !argv_of(front).is_empty(),
            "a live process has an argv, whether or not it leads its group"
        );
    }

    /// Against the process running this test, so the reading is exercised and
    /// not only the parsing.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_directory_read_out_of_proc_is_the_one_the_process_is_in() {
        let me = std::process::id();
        let here = std::env::current_dir().unwrap().into_os_string();
        assert_eq!(cwd_of(me), here.into_string().ok());

        assert!(
            argv_of(me).iter().all(|arg| !arg.is_empty()),
            "and no empty words in the argv"
        );
    }

    /// A pid that cannot exist answers blank at every step, because a session
    /// ending between being listed and being read is ordinary.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_process_that_is_gone_is_a_blank_rather_than_a_failure() {
        assert_eq!(cwd_of(u32::MAX), None);
        assert!(argv_of(u32::MAX).is_empty());
        assert_eq!(
            in_front_of(u32::MAX),
            u32::MAX,
            "nothing to read, so itself"
        );
        assert_eq!(of(u32::MAX), Foreground::default());
    }
}
