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
//! Linux only, and it answers a blank rather than a guess anywhere else. macOS
//! has no `/proc`: the directory would come from `libproc` and the argv from a
//! `KERN_PROCARGS2` sysctl, which is a separate piece of work. A blank is a
//! real answer here, and a much better one than a wrong directory: a session
//! restored in the wrong place resumes somebody else's conversation.

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
    let front = in_front_of(leader).unwrap_or(leader);
    Foreground {
        // The foreground process is where the work is, and it is free to have
        // moved: a `cd` in a subshell, a build started from somewhere else.
        // The leader's own directory is the fallback, not the answer.
        cwd: cwd_of(front).or_else(|| cwd_of(leader)),
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

/// The process group in front of the session's terminal.
///
/// `None` when the answer is not usable: a negative `tpgid` is a process with
/// no controlling terminal, and a zero one is no foreground group at all.
#[cfg(target_os = "linux")]
fn in_front_of(leader: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{leader}/stat")).ok()?;
    let tpgid = tpgid(&stat)?;
    (tpgid > 0).then_some(tpgid as u32)
}

/// The `tpgid` field of a `/proc/<pid>/stat` line.
///
/// Read from after the *last* `)` rather than by splitting the line from the
/// left, because the second field is the executable's name in parentheses and
/// an executable is free to be called `foo (old) (2)`. Counting columns from
/// the left walks into the middle of that name and reads a different field,
/// for those processes only, which is the kind of bug that works on every
/// machine it is tested on.
#[cfg(target_os = "linux")]
fn tpgid(stat: &str) -> Option<i32> {
    let after = &stat[stat.rfind(')')? + 1..];
    // What follows the name is state, ppid, pgrp, session, tty_nr, tpgid.
    after.split_whitespace().nth(5)?.parse().ok()
}

/// Where a process is, or `None` if that cannot be read as a path.
///
/// A directory that has been deleted out from under the process reads as
/// `<path> (deleted)`, which is the kernel annotating rather than answering.
/// Restoring into a directory of that literal name is not what anybody meant,
/// so it is refused: no directory beats a wrong one everywhere here.
#[cfg(target_os = "linux")]
fn cwd_of(pid: u32) -> Option<String> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    // Not lossy: a path that is not UTF-8 cannot be put in a `SpawnSpec`, and
    // a lossily mangled one names a directory that does not exist.
    let path = path.into_os_string().into_string().ok()?;
    (!path.ends_with(" (deleted)")).then_some(path)
}

/// The argv of a process, as separate words.
///
/// Empty words are dropped, which is not tidying. A program that rewrites its
/// own process title writes over the whole argv block and pads the rest of it
/// with NULs: `pi` does, so read literally its argv is `pi` and seventy-five
/// empty arguments. Handed back to a restore, those are quoted into `''` and
/// passed to the program as arguments it never received.
///
/// The words themselves are left exactly as they are. An earlier version
/// trimmed each one, on a misreading of that padding as spaces, which would
/// have quietly changed `rg ' foo '` and `git commit -m 'wip '` into different
/// commands on the way back.
#[cfg(target_os = "linux")]
fn argv_of(pid: u32) -> Vec<String> {
    let Ok(raw) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
        return Vec::new();
    };
    String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|arg| !arg.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(all(test, target_os = "linux"))]
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

    /// The equality that is not a signal. A shell `exec`s its final command,
    /// so a session started with one has the command in the foreground under
    /// the pid the node recorded. An earlier draft read that as "nothing is
    /// running" and threw the command away on every such session.
    #[test]
    fn a_session_whose_command_replaced_its_shell_is_still_running_that_command() {
        let me = std::process::id();
        // The test binary is its own process group leader in the usual case;
        // whatever the answer, asking about ourselves gives our own argv back
        // rather than an empty one.
        let front = in_front_of(me).unwrap_or(me);
        assert!(
            !argv_of(front).is_empty(),
            "a live process has an argv, whether or not it leads its group"
        );
    }

    /// Against the process running this test, so the reading is exercised and
    /// not only the parsing.
    #[test]
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
    fn a_process_that_is_gone_is_a_blank_rather_than_a_failure() {
        assert_eq!(cwd_of(u32::MAX), None);
        assert!(argv_of(u32::MAX).is_empty());
        assert_eq!(in_front_of(u32::MAX), None);
        assert_eq!(of(u32::MAX), Foreground::default());
    }
}
