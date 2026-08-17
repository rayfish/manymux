//! What every session was doing, written down well enough to start it again.
//!
//! The problem this exists for: the node keeps executing the build it started
//! from, so picking up a new binary means restarting it, and a restart is the
//! end of every session on that machine. There is no way round that, since the
//! sessions are the node's children and hold PTYs it owns. What there can be
//! is a way to write down where each one was and what it was running, and to
//! start them again afterwards.
//!
//! Kept in the client, beside `groups.toml`, and for the same reasons.
//!
//! A group is here because membership is yours rather than a property of the
//! session, and a checkpoint inherits that: the group each session was in is
//! part of what has to come back, and only this end knows it. A node cannot
//! restore a grouping it has never been told about, which is why a checkpoint
//! taken from the machine you are sitting at is the one that puts your `mm ls`
//! back the way it was. Taken *on* a host, the sessions come back and the
//! groups come back only as far as that host's own `groups.toml` knows them.
//!
//! The resume table is here for the second reason a group is: it grows every
//! few months as another program learns to continue where it left off, and
//! defined at the node it would grow only on the machines that had been
//! restarted, which are exactly the ones that did not need it.
//!
//! And the awkward one, which is a consequence rather than a choice: on any
//! machine, the *first* checkpoint has to be taken without asking that
//! machine's node anything, because the node holding the sessions worth saving
//! is by definition running the build from before this existed and refuses the
//! question. That is why [`crate::foreground`] is in the library rather than
//! under `node`: a client on the same machine reads `/proc` itself, from the
//! pid the listing already carries, and needs nothing from the node but the
//! listing it has always given.
//!
//! A machine reached over ssh is read the same way, over the connection that
//! is already open ([`read_proc`], [`read_back`]). The far end `cat`s and
//! `readlink`s and decides nothing; every rule about what that means is
//! applied here, on the same functions the local path uses. That is not
//! tidiness: a shell script that worked out which process was in front, or
//! what a ` (deleted)` suffix meant, would be a second implementation of rules
//! that took three review rounds to get right, and it would drift.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

/// A program that can be asked to pick up where it left off.
pub struct Resumable {
    pub program: &'static str,
    /// What to add to make it continue rather than start fresh.
    pub flag: &'static str,
    /// Flags that already say which conversation to pick up. Adding anything
    /// beside one of these is arguing with a choice somebody already made, and
    /// these programs reject the combination rather than pick a winner.
    pub already: &'static [&'static str],
    /// The ones out of [`already`](Self::already) that take the conversation as
    /// an argument, and open a chooser when they are not given one. Bare, such
    /// a flag has not answered the question, it has asked it, and a restore has
    /// nobody to ask.
    pub picks: &'static [&'static str],
    /// Runs that are not a conversation and have nothing to continue. Named
    /// rather than guessed at from the shape, since the same position takes an
    /// opening prompt.
    pub subcommands: &'static [&'static str],
}

/// The programs worth knowing about, and how to continue them.
///
/// Both resume the newest session *in the current directory*, which is the
/// whole reason the directory is the part of a checkpoint that must not be
/// guessed at: run in the wrong one, the flag does not fail, it picks up
/// somebody else's conversation.
const RESUMED: &[Resumable] = &[
    Resumable {
        program: "claude",
        flag: "--continue",
        already: &[
            "--continue",
            "-c",
            "--resume",
            "-r",
            "--session-id",
            "--from-pr",
            "--teleport",
        ],
        picks: &["--resume", "-r"],
        subcommands: &[
            "mcp",
            "config",
            "doctor",
            "update",
            "install",
            "migrate-installer",
            "setup-token",
            "plugin",
        ],
    },
    Resumable {
        program: "pi",
        flag: "--continue",
        already: &[
            "--continue",
            "-c",
            "--resume",
            "-r",
            "--session",
            "--session-id",
            "--fork",
        ],
        picks: &["--resume", "-r"],
        subcommands: &[
            "install",
            "remove",
            "uninstall",
            "update",
            "list",
            "config",
            "auth",
        ],
    },
];

/// Shells, for telling a session sitting at a prompt from one running a
/// program. A shell not on this list falls through to being run again, which
/// lands you in that shell in the same directory: a harmless way to be wrong,
/// which is what makes a list safe to be a list.
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "dash", "ksh", "mksh", "tcsh", "csh", "nu", "elvish", "xonsh",
];

/// Flags that mean a shell was handed a command rather than a keyboard. Such a
/// session is running that command, not sitting at a prompt.
///
/// `-mc` is ours, from [`to_spawn`]; the rest are the spellings a login shell
/// and a `-c` invocation come in.
const SHELL_COMMAND_FLAGS: &[&str] = &[
    "-c",
    "-lc",
    "-ic",
    "-lic",
    "-ilc",
    "-mc",
    "-lmc",
    "-mlc",
    "--command",
];

/// One session, written down well enough to start it again.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub host: String,
    /// The name it had, which is the name it gets back. A restore happens into
    /// a machine those sessions have left, so nothing is competing for it, and
    /// a name that *is* taken means that session is already back.
    pub name: String,
    /// Where it was. Not optional: a session put back in the wrong directory
    /// is worse than one not put back at all, so an entry whose directory
    /// could not be read is never written.
    pub cwd: String,
    /// The group it was in. Only the name is worth keeping, since membership
    /// is keyed on a pid and a restore invents a new one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// What to run, worked out when this was written rather than when it is
    /// read, so the file says what will happen and can be edited before it
    /// does. Empty is the login shell, which is what a session sitting at a
    /// prompt comes back as.
    #[serde(default)]
    pub command: Vec<String>,
}

/// Every session worth putting back, in the order they were started in.
#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Checkpoint {
    /// Seconds since the epoch, kept the way a group member's start time is,
    /// so `mm checkpoint show` can say how old this is. Before the sessions
    /// because TOML writes plain values before tables, and an array of tables
    /// is tables.
    #[serde(default)]
    pub taken: u64,
    /// A list rather than a map, because this is the one file here whose order
    /// means something: a session's place in every listing is when it was
    /// started, so restoring in file order is what puts the rows back where
    /// they were.
    #[serde(default)]
    pub sessions: Vec<Kept>,
}

impl Checkpoint {
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding the checkpoint")?;
        config::write_private_file(&Self::path(), text.as_bytes())
    }

    /// Its own file rather than a section in `settings.toml`, the reason
    /// [`super::groups::Groups`] gives: the settings are hand-edited and this
    /// is written by a command.
    pub fn path() -> PathBuf {
        config::config_dir().join("checkpoint.toml")
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Now, for stamping a checkpoint as it is written.
    pub fn now() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0)
    }
}

/// What to run to put a session back, given the argv that was in front of its
/// terminal.
///
/// Three answers and no guessing between them: a shell at a prompt comes back
/// as a login shell, a program that can resume itself is asked to, and
/// anything else is run again exactly as it was. The last is the one worth
/// stating plainly, because it is a decision rather than a fallback: a session
/// is a piece of work, and the command it was running is the best description
/// of that work anyone here has.
pub fn resumed(foreground: &[String]) -> Vec<String> {
    // Peeled in this order because they nest that way on a shell that does not
    // `exec`: the login shell's `-lc` is outermost, and a wrapper a previous
    // restore added sits inside it.
    let mut argv = unwrapped(foreground).to_vec();
    // A loop rather than one pass because these nest: a login shell that did
    // not get out of the way holds a `-lc`, and a wrapper of ours can sit
    // inside it. Each pass reads strictly inward, so it ends when there is
    // nothing left to read; the equality is a stop for the pathological case
    // where it would not shrink.
    while let Some(inner) = shell_command(&argv) {
        let inner = unwrapped(&inner).to_vec();
        if inner == argv {
            break;
        }
        argv = inner;
    }
    if at_a_prompt(&argv) {
        return Vec::new();
    }
    if let Some(resumable) = resumable(&argv)
        && !a_subcommand(resumable, &argv)
    {
        match resuming(resumable, &argv) {
            Resuming::Named => {}
            Resuming::Fresh => argv.push(resumable.flag.to_string()),
            Resuming::Chosen(at) => {
                argv.remove(at);
                argv.push(resumable.flag.to_string());
            }
        }
    }
    argv
}

/// What an argv says about picking a conversation up.
enum Resuming {
    /// It says which one, so nothing here may argue with it.
    Named,
    /// It asks for one to be chosen by hand, at the word given. A restore has
    /// nobody to ask, so this is taken out and replaced.
    Chosen(usize),
    /// It says nothing, so the flag is added.
    Fresh,
}

/// Which of those three an argv is.
///
/// The distinction that matters is between a flag that answers "which
/// conversation" and one that asks it. `claude --resume 7f3a…` has answered,
/// and `--continue` beside it is a second answer to a settled question, which
/// claude rejects rather than picking between; that is what [`Resumable::already`]
/// is for and it was found in a real session on the first machine this was
/// pointed at. But `claude --resume` on its own has answered nothing: it opens
/// a chooser and waits for somebody to walk it. Restored as it stands, the
/// session comes back sitting at that menu, having resumed nothing, which is
/// the one outcome a checkpoint exists to avoid — and it comes back that way
/// silently, since a menu on the screen looks like a program that started.
///
/// So a bare picker flag is dropped and the resume-the-newest flag put in its
/// place. That is the same thing the person meant: they were resuming the
/// conversation in this directory, and `--continue` is how to say so without a
/// keyboard. It is a rewrite of what was captured rather than a faithful
/// record, which the rest of this module refuses to do, and it earns the
/// exception by being the only case where running the command as captured
/// cannot do what the command was doing.
///
/// A flag is matched whole or up to its `=`, so `--session-id=abc` counts and a
/// hypothetical `--continue-on-error` does not. A picker flag counts as having
/// been given a conversation when it was spelled with an `=` or the next word
/// is not another flag.
fn resuming(resumable: &Resumable, argv: &[String]) -> Resuming {
    let mut chooser = None;
    for (at, arg) in argv.iter().enumerate().skip(1) {
        let (name, valued) = match arg.split_once('=') {
            Some((name, _)) => (name, true),
            None => (arg.as_str(), false),
        };
        if !resumable.already.contains(&name) {
            continue;
        }
        if !resumable.picks.contains(&name) {
            return Resuming::Named;
        }
        // Its argument is optional, so what decides is whether it was given
        // one. A flag after it is the next option rather than a conversation.
        let told_which = valued || argv.get(at + 1).is_some_and(|next| !next.starts_with('-'));
        if told_which {
            return Resuming::Named;
        }
        chooser = Some(at);
    }
    chooser.map_or(Resuming::Fresh, Resuming::Chosen)
}

/// Whether this run is one of the program's subcommands rather than a
/// conversation: `claude mcp serve`, `pi install …`. Such a run has nothing to
/// continue, and the flag added to one is rejected rather than ignored, so the
/// session comes back as a bare shell.
///
/// Matched against the names, not against "the first word is not a flag". Both
/// of these also take an opening prompt in that position, and `claude 'review
/// the PR'` read as a subcommand is the failure this is meant to prevent
/// arriving by the other door: no resume flag, so the restore starts a fresh
/// conversation *and* hands it the prompt again as work to do.
fn a_subcommand(resumable: &Resumable, argv: &[String]) -> bool {
    argv.get(1)
        .is_some_and(|arg| resumable.subcommands.contains(&arg.as_str()))
}

/// Whether this argv is a shell waiting for somebody to type, rather than
/// something running.
///
/// Not decided by comparing the foreground pid to the session's, which was the
/// first and wrong answer: bash and zsh `exec` a simple final command, so a
/// session started as `mm new box claude` *is* claude under the pid the node
/// recorded. Read that way, every session started with a command looked like
/// an empty prompt and its command was thrown away.
fn at_a_prompt(argv: &[String]) -> bool {
    let Some(program) = argv.first().map(|p| basename(p)) else {
        // Nothing known: not a prompt, and nothing to run either. It comes
        // back as a login shell, which is what an empty command means.
        return true;
    };
    SHELLS.contains(&program.as_str()) && !handed_a_command(argv)
}

/// Whether a shell's argv says it was handed a command rather than a keyboard.
fn handed_a_command(argv: &[String]) -> bool {
    argv[1..]
        .iter()
        .any(|arg| SHELL_COMMAND_FLAGS.contains(&arg.as_str()))
}

/// The command inside a `shell -lc '<command>'`, when that is a plain one.
///
/// The node runs every spawn through the login shell, and only *some* shells
/// then get out of the way: bash and zsh `exec` a simple final command, dash
/// does not. So on a machine whose login shell is `/bin/sh` — Debian's dash,
/// the passwd shell of most deploy accounts, and `user::shell`'s own last
/// resort — the foreground of `mm new box claude` is `["/bin/sh", "-lc",
/// "claude"]` rather than `["claude"]`.
///
/// Left unread, that is the failure this whole feature exists to prevent, and
/// a silent one: `claude` is not the program at `argv[0]`, so no resume flag is
/// added and the session comes back on a fresh conversation. It also nests,
/// since the wrapper a restore adds ends up inside the next capture's snippet.
///
/// Only a plain command is read back, meaning one with nothing in it a shell
/// would treat as syntax. A snippet with a pipe or a `;` in it is left exactly
/// as it is and run through a shell again, because the words of `a | b` are
/// not an argv and quoting them as one would exec a program with that name.
fn shell_command(argv: &[String]) -> Option<Vec<String>> {
    let program = basename(argv.first()?);
    if !SHELLS.contains(&program.as_str()) {
        return None;
    }
    // The word straight after the flag, and nothing may follow it. What would
    // follow is `$0` and the positional parameters, which is a shape this does
    // not claim to read — and taking the *last* word instead, as this once
    // did, reads one of those parameters as the command: `sh -c 'sleep 900'
    // dummy hello` came back as `hello`, which a restore would then try to
    // exec as a program of that name.
    let flag = argv[1..]
        .iter()
        .position(|arg| SHELL_COMMAND_FLAGS.contains(&arg.as_str()))?
        + 1;
    if flag + 2 != argv.len() {
        return None;
    }
    let snippet = &argv[flag + 1];
    if snippet.contains(|c| "|&;<>()$`\\\"'*?[#~=%{}\n\t".contains(c)) {
        return None;
    }
    let words: Vec<String> = snippet.split_whitespace().map(str::to_string).collect();
    (!words.is_empty()).then_some(words)
}

/// How to resume this program, if it is one that can be.
fn resumable(argv: &[String]) -> Option<&'static Resumable> {
    let program = basename(argv.first()?);
    RESUMED.iter().find(|known| known.program == program)
}

/// Whether a command picks up its conversation from the directory it is run
/// in, which is what makes two of them in one directory a collision.
///
/// Both of the programs known here do, so this is "is it one of them" today.
/// It is asked as its own question because that is the property that matters,
/// and a program that resumed by some other means would answer differently.
pub fn resumes_by_directory(command: &[String]) -> bool {
    resumable(command).is_some()
}

/// The program out of a path, with the leading `-` a login shell is given.
fn basename(program: &str) -> String {
    let name = std::path::Path::new(program)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| program.to_string());
    name.trim_start_matches('-').to_string()
}

/// The script a restored session runs: the captured command, then the login
/// shell in its place.
///
/// `"$@"` is the words themselves, so nothing here has to quote anything.
const KEEP_THE_SHELL: &str = r#""$@"; exec "${SHELL:-/bin/sh}" -l"#;

/// The spawn command that runs `command` and leaves the session behind it.
///
/// The node runs a spawn as `shell -lc <command>`, so a session restored as
/// `claude --continue` **ends when claude exits**. What was there before was a
/// login shell somebody started claude from, where quitting it leaves you at a
/// prompt in the same directory, and a restore that quietly changed that would
/// take the session away at the moment somebody pressed the key to leave a
/// program.
///
/// So the captured argv is passed to `sh` as positional parameters and the
/// login shell is `exec`ed after it. Each word arrives as its own parameter,
/// which is what keeps the quoting out of here: `SpawnSpec::command` is joined
/// with a shell quoter at the far end and `"$@"` puts the words back.
///
/// `-m` is the load-bearing letter. Job control is what puts the command in a
/// process group of its own and hands it the terminal, so `/proc` reports the
/// *program* as the session's foreground rather than this wrapper. Without it
/// the wrapper is what the next checkpoint sees, and restoring that wraps it
/// again: one shell deeper every save-restore cycle, without bound. A test
/// holds the round trip to a fixed point. It is also what makes a Ctrl-C reach
/// the program rather than the thing that started it.
///
/// And the leading `exec` is what makes any of that reachable. All of this
/// runs inside the node's own `shell -lc …`, and job control here only helps
/// if that shell got out of the way first. Some do it by themselves for a
/// simple final command and some never do, which was known; what was not is
/// that the same shell does it on one machine and not on another. bash `exec`s
/// this on a developer's box and did not on a CI runner, because a trap set by
/// a profile turns the optimisation off, and the sessions there came back
/// wrapped one layer deeper each time. Saying `exec` outright costs nothing
/// and holds on bash, dash and zsh alike, which is measured rather than
/// assumed.
pub fn to_spawn(command: &[String]) -> Vec<String> {
    if command.is_empty() {
        // Already "the login shell" as far as a spawn is concerned.
        return Vec::new();
    }
    let mut argv = vec![
        "exec".to_string(),
        "sh".to_string(),
        "-mc".to_string(),
        KEEP_THE_SHELL.to_string(),
        // `sh` again: `$0` for the shell running the script, which is not one
        // of the words to run and would otherwise eat the program's name.
        "sh".to_string(),
    ];
    argv.extend_from_slice(command);
    argv
}

/// The command inside one of our own wrappers, or the argv unchanged.
///
/// Beside the `-m` in [`to_spawn`], which is what actually keeps the wrapper
/// out of a checkpoint: the command it starts takes the terminal, so the
/// wrapper is not what `/proc` reports. This covers the window before that has
/// happened, and covers it only where the login shell `exec`ed the wrapper. On
/// a shell that did not, the foreground in that window is the login shell's
/// own `-lc` with all of this inside its snippet, which nothing here will
/// read. The window is short, so that is a hole worth knowing about rather
/// than one worth more machinery.
///
/// Without either, the *second* checkpoint of a session would write down the
/// wrapper and restoring that would wrap it again, one layer deeper every time
/// round.
///
/// Recognised by its exact shape, which is safe because we are the only thing
/// that writes it. A shell script of somebody's own that happens to look like
/// this is the same command either way.
fn unwrapped(argv: &[String]) -> &[String] {
    match argv {
        [sh, flag, script, argv0, rest @ ..]
            if sh == "sh" && flag == "-mc" && script == KEEP_THE_SHELL && argv0 == "sh" =>
        {
            rest
        }
        _ => argv,
    }
}

/// A shell command that prints one `/proc` file per pid, and decides nothing.
///
/// The far end reads and this end parses, which is the whole point: a script
/// that worked out which process was in front, or what a `(deleted)` suffix
/// meant, would be a second copy of rules that took three review rounds to get
/// right and would drift from the first the week after. So it `cat`s and it
/// `readlink`s, and that is all it does.
///
/// Hex for the contents because a `cmdline` is NUL-separated and need not be
/// UTF-8, neither of which survives a line of shell output. `od` is POSIX, so
/// this needs nothing installed. A pid that has gone prints an empty body
/// rather than failing, since a session ending between two questions is
/// ordinary.
pub fn read_proc(pids: &[u32], want: Want) -> String {
    let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
    // `P` opens a record and `X` carries the bytes, so a pid that answers
    // nothing is still a record and still lines up with what was asked about.
    format!(
        "for p in {}; do printf 'P %s\\n' \"$p\"; \
         printf 'X %s\\n' \"$({} | od -An -v -tx1 | tr -d ' \\n')\"; done",
        pids.join(" "),
        want.reader()
    )
}

/// Which `/proc` file a pass is after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Want {
    /// The leader's, for the process group in front of its terminal.
    Stat,
    /// Where that process is. A link rather than a file, so `readlink`, and it
    /// writes a newline that `std::fs::read_link` does not: hex-encoded with
    /// the rest, that newline lands on the end of the directory and the two
    /// ways of reading the same process stop agreeing. The inner `printf %s`
    /// is what takes it off, and takes off only the trailing ones, so a path
    /// with a newline inside it is still carried exactly.
    Cwd,
    /// Its argv, NUL-separated and not necessarily UTF-8, which is why all of
    /// this is hex in the first place.
    Cmdline,
}

impl Want {
    fn reader(self) -> &'static str {
        match self {
            Self::Stat => "cat /proc/$p/stat 2>/dev/null",
            Self::Cwd => "printf %s \"$(readlink /proc/$p/cwd 2>/dev/null)\"",
            Self::Cmdline => "cat /proc/$p/cmdline 2>/dev/null",
        }
    }
}

/// The bytes each pid answered with, in the order the records arrived.
///
/// Anything that is not a record this wrote is skipped rather than guessed at:
/// a login shell on the far side is free to print a banner before the command
/// runs, and a checkpoint is not the place to find out that somebody's
/// `.profile` says hello.
pub fn read_back(out: &[u8]) -> Vec<(u32, Vec<u8>)> {
    let text = String::from_utf8_lossy(out);
    let mut found = Vec::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("P ") {
            pid = rest.trim().parse::<u32>().ok();
        } else if let Some(rest) = line.strip_prefix("X ")
            && let Some(p) = pid.take()
        {
            found.push((p, unhex(rest.trim())));
        }
    }
    found
}

fn unhex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    digits
        .chunks_exact(2)
        .filter_map(|pair| u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok())
        .collect()
}

/// Whether a wrapper of ours is still in this command after everything above
/// has had a go at reading through it.
///
/// Asked of the *answer*, never of the raw foreground. The plain wrapper shape
/// carries the marker too and [`unwrapped`] reads straight through it; what is
/// left here is the shape nothing can read, the wrapper quoted inside a login
/// shell's own `-lc` snippet, where it is not an argv any more.
///
/// Which is a session still starting up, almost always. The node runs every
/// spawn through a login shell, and that shell reads its profile before it
/// reaches the `exec`: ask in that window and the login shell is what holds
/// the terminal. It is short, and long enough to hit every time on a slow
/// machine when a save follows a restore immediately, which is how it was
/// found.
///
/// Recorded rather than refused, it would be a session whose command gains a
/// shell every time it is saved and restored, without bound, and that one does
/// not come back. So the caller writes it down as one it could not describe,
/// which is reported and counts against the save, and asking again a moment
/// later gets the real answer.
pub fn still_wrapped(argv: &[String]) -> bool {
    argv.iter().any(|word| word.contains(KEEP_THE_SHELL))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_known_agent_comes_back_asked_to_continue() {
        assert_eq!(resumed(&argv(&["claude"])), argv(&["claude", "--continue"]));
        assert_eq!(resumed(&argv(&["pi"])), argv(&["pi", "--continue"]));
        assert_eq!(
            resumed(&argv(&["/usr/local/bin/claude"])),
            argv(&["/usr/local/bin/claude", "--continue"]),
            "found by its name and started by the path it was started with"
        );
    }

    /// The flags somebody chose are part of how they were working, so they
    /// survive: coming back without `--dangerously-skip-permissions` would be
    /// a different session that happened to share a directory.
    #[test]
    fn the_flags_a_session_was_started_with_survive_the_resume() {
        assert_eq!(
            resumed(&argv(&["claude", "--dangerously-skip-permissions"])),
            argv(&["claude", "--dangerously-skip-permissions", "--continue"])
        );
    }

    /// Or a session checkpointed twice would accumulate them.
    #[test]
    fn an_agent_already_asked_to_continue_is_not_asked_twice() {
        assert_eq!(
            resumed(&argv(&["claude", "--continue"])),
            argv(&["claude", "--continue"])
        );
        assert_eq!(
            resumed(&resumed(&argv(&["pi"]))),
            argv(&["pi", "--continue"]),
            "a checkpoint of a restored session is the same checkpoint"
        );
    }

    /// Somebody who said which conversation to pick up has already answered
    /// the question, and the two answers do not combine: claude rejects
    /// `--resume <id> --continue` rather than preferring one. Found in a real
    /// session on the first machine this was pointed at.
    #[test]
    fn a_session_already_told_which_conversation_to_pick_up_is_left_alone() {
        for chosen in ["-c", "--session-id", "--from-pr", "--teleport"] {
            let started = argv(&["claude", "--dangerously-skip-permissions", chosen]);
            assert_eq!(
                resumed(&started),
                started,
                "{chosen} already says which one, so nothing is added"
            );
        }

        // The picker flags, given the conversation they take.
        for named in [
            argv(&["claude", "--resume", "7f3a-9c21"]),
            argv(&["claude", "--resume=7f3a-9c21"]),
            argv(&["claude", "-r", "7f3a-9c21"]),
            argv(&["pi", "--session=abc123"]),
        ] {
            assert_eq!(resumed(&named), named, "{named:?} names one already");
        }

        // And a flag that merely starts the same way is not that flag.
        assert_eq!(
            resumed(&argv(&["claude", "--continue-on-error"])),
            argv(&["claude", "--continue-on-error", "--continue"])
        );
    }

    /// A bare `--resume` opens a chooser and waits for somebody to walk it,
    /// which is the opposite of what a restore needs: nobody is watching, so
    /// the session comes back sitting at a menu having resumed nothing, and it
    /// looks from outside exactly like a program that started.
    ///
    /// What that person meant is the conversation in this directory, and
    /// `--continue` is how to say so without a keyboard. So the chooser is
    /// taken out and replaced. This is the one place the module rewrites what
    /// it captured, and it earns it: running the command as captured cannot do
    /// what the command was doing.
    #[test]
    fn a_resume_with_nothing_to_resume_becomes_a_continue() {
        assert_eq!(
            resumed(&argv(&[
                "claude",
                "--dangerously-skip-permissions",
                "--resume"
            ])),
            argv(&["claude", "--dangerously-skip-permissions", "--continue"]),
            "the real session that prompted this"
        );

        for bare in ["--resume", "-r"] {
            assert_eq!(
                resumed(&argv(&["claude", bare])),
                argv(&["claude", "--continue"]),
                "{bare} on its own asks the question rather than answering it"
            );
            assert_eq!(resumed(&argv(&["pi", bare])), argv(&["pi", "--continue"]));
        }

        // The word after it is another option, so it still names nothing.
        assert_eq!(
            resumed(&argv(&[
                "claude",
                "--resume",
                "--dangerously-skip-permissions"
            ])),
            argv(&["claude", "--dangerously-skip-permissions", "--continue"])
        );

        // And what comes back is a fixed point: checkpointing a restored
        // session must not walk the flags around again.
        let once = resumed(&argv(&["claude", "--resume"]));
        assert_eq!(resumed(&once), once);
    }

    /// A subcommand is not a conversation, and the flag added to one is
    /// rejected rather than ignored, so the session comes back as a bare
    /// shell instead of as the thing that was running.
    #[test]
    fn a_subcommand_is_not_asked_to_continue() {
        let serving = argv(&["claude", "mcp", "serve"]);
        assert_eq!(resumed(&serving), serving);
        let installing = argv(&["pi", "install", "something"]);
        assert_eq!(resumed(&installing), installing);

        // A flag first is the ordinary interactive run, which is continued.
        assert_eq!(
            resumed(&argv(&["claude", "--dangerously-skip-permissions"])),
            argv(&["claude", "--dangerously-skip-permissions", "--continue"])
        );
    }

    /// The same position takes an opening prompt, and that is a conversation
    /// like any other. Read as a subcommand it gets no resume flag, so the
    /// restore starts a fresh conversation *and* hands it the prompt again as
    /// work to do, which for a session running without permission checks means
    /// doing it twice.
    #[test]
    fn an_opening_prompt_is_not_a_subcommand() {
        assert_eq!(
            resumed(&argv(&["claude", "review the PR"])),
            argv(&["claude", "review the PR", "--continue"])
        );
        assert_eq!(
            resumed(&argv(&["pi", "fix the parser"])),
            argv(&["pi", "fix the parser", "--continue"])
        );
        // And a word that is a subcommand of the *other* program is not one
        // of this program's.
        assert_eq!(
            resumed(&argv(&["claude", "auth"])),
            argv(&["claude", "auth", "--continue"])
        );
    }

    /// The snippet is the word after the flag and nothing may follow it.
    /// Reading the *last* word instead, as this once did, takes a positional
    /// parameter for the command: a restore would then exec a program named
    /// after whatever the script's last argument happened to be.
    #[test]
    fn a_positional_parameter_is_not_mistaken_for_the_command() {
        let with_params = argv(&["sh", "-c", "sleep 900", "dummy", "hello"]);
        assert_eq!(
            resumed(&with_params),
            with_params,
            "an unreadable shape is left exactly as it was, not half-read"
        );

        // The shape it does claim to read, with nothing after the snippet.
        assert_eq!(
            resumed(&argv(&["/bin/sh", "-lc", "claude"])),
            argv(&["claude", "--continue"])
        );
    }

    /// The property that makes two sessions in one directory a collision worth
    /// warning about, and the reason the directory may never be guessed at.
    #[test]
    fn the_programs_that_resume_are_the_ones_that_read_the_directory() {
        assert!(resumes_by_directory(&argv(&["claude", "--continue"])));
        assert!(resumes_by_directory(&argv(&["pi"])));
        assert!(!resumes_by_directory(&argv(&["cargo", "watch"])));
        assert!(!resumes_by_directory(&[]), "a login shell resumes nothing");
    }

    #[test]
    fn a_program_nobody_here_has_heard_of_is_run_again_exactly_as_it_was() {
        assert_eq!(
            resumed(&argv(&["cargo", "watch", "-x", "test"])),
            argv(&["cargo", "watch", "-x", "test"])
        );
        assert_eq!(
            resumed(&argv(&["vim", "src/main.rs"])),
            argv(&["vim", "src/main.rs"])
        );
    }

    #[test]
    fn a_shell_at_a_prompt_comes_back_as_a_login_shell_and_not_as_a_shell_running_a_shell() {
        assert!(resumed(&argv(&["/usr/bin/zsh", "-l"])).is_empty());
        assert!(
            resumed(&argv(&["-zsh"])).is_empty(),
            "a login shell's argv0"
        );
        assert!(resumed(&argv(&["bash"])).is_empty());
        assert!(
            resumed(&[]).is_empty(),
            "and nothing known is not a command"
        );
    }

    /// The other half of that, and the case the pid test got wrong: a shell
    /// handed a command is running that command.
    ///
    /// A snippet with shell syntax in it stays wrapped, because the words of
    /// `a | b` are not an argv and quoting them as one would exec a program
    /// with that name.
    #[test]
    fn a_shell_running_a_command_is_the_command_and_not_a_prompt() {
        let running = argv(&["/usr/bin/zsh", "-lc", "cargo build | tee log"]);
        assert_eq!(
            resumed(&running),
            running,
            "left as it was, wrapper and all"
        );
    }

    /// The shell the node wraps every spawn in only gets out of the way on
    /// some machines: bash and zsh `exec` a simple final command, dash does
    /// not, and dash is `/bin/sh` on Debian and the passwd shell of most
    /// deploy accounts. So on those the foreground of `mm new box claude` is
    /// the wrapper, and reading it literally means no resume flag is added and
    /// the session comes back on a fresh conversation. Silently, which is the
    /// one failure the whole feature exists to prevent.
    #[test]
    fn a_shell_that_did_not_get_out_of_the_way_is_read_through() {
        assert_eq!(
            resumed(&argv(&["/bin/sh", "-lc", "claude"])),
            argv(&["claude", "--continue"]),
            "the command is claude, however many shells are standing in front of it"
        );
        assert_eq!(
            resumed(&argv(&[
                "/bin/sh",
                "-lc",
                "claude --dangerously-skip-permissions"
            ])),
            argv(&["claude", "--dangerously-skip-permissions", "--continue"])
        );
        // And one already told which conversation to pick up is still left be.
        let chosen = argv(&["/bin/sh", "-lc", "claude --resume 7f3a"]);
        assert_eq!(resumed(&chosen), argv(&["claude", "--resume", "7f3a"]));

        // A login shell with no command is still a prompt.
        assert!(resumed(&argv(&["/bin/sh", "-l"])).is_empty());
    }

    /// And the round trip has to be a fixed point there too, or the wrapper a
    /// restore adds ends up inside the next capture's snippet and the command
    /// sinks one shell deeper every cycle, without bound.
    /// What the `-m` in `to_spawn` buys, and why it is not cosmetic.
    ///
    /// Job control puts the command in a process group of its own and gives it
    /// the terminal, so what a later checkpoint reads out of `/proc` is the
    /// program rather than the wrapper that started it. Verified against a
    /// real node: a restored `sleep 3000` reports `tpgid` pointing at `sleep`,
    /// and the save after it writes `["sleep", "3000"]` again.
    #[test]
    fn a_restored_session_reports_the_program_rather_than_the_wrapper() {
        let first = resumed(&argv(&["/bin/sh", "-lc", "claude"]));
        assert_eq!(first, argv(&["claude", "--continue"]));

        let ran = to_spawn(&first);
        assert_eq!(ran[0], "exec", "so the login shell gets out of the way");
        assert_eq!(ran[2], "-mc", "job control is what makes the rest true");

        // What `/proc` reports for that session, thanks to the two lines
        // above: the words after `$0`, which are the command itself.
        let reported = ran[5..].to_vec();
        assert_eq!(reported, first);
        assert_eq!(
            resumed(&reported),
            first,
            "so the cycle settles instead of sinking a shell deeper each time"
        );
    }

    /// The cycle has to be stable, or every checkpoint of an already-restored
    /// session buries the command one wrapper deeper.
    #[test]
    fn a_checkpoint_of_a_restored_session_is_the_same_checkpoint() {
        let first = resumed(&argv(&["claude", "--dangerously-skip-permissions"]));
        let ran = to_spawn(&first);

        // The plain shape, which is what `/proc` shows in the window before
        // job control has handed the terminal over.
        let bare = ran[1..].to_vec();
        let second = resumed(&bare);
        assert_eq!(second, first, "the wrapper is seen through, not recorded");
        assert_eq!(to_spawn(&second), ran, "so the round trip is a fixed point");

        // And a session whose program has since exited, leaving the login
        // shell the wrapper exec'd, is a prompt again.
        assert!(resumed(&argv(&["/usr/bin/zsh", "-l"])).is_empty());
    }

    /// The shape that CI found and a developer's machine could not: a login
    /// shell that never got out of the way, leaving the wrapper quoted inside
    /// its `-lc` snippet where it is no longer an argv. bash does this when a
    /// profile has set a trap, which turns off the `exec` it would otherwise
    /// do for a simple final command, so the same binary behaves one way on a
    /// laptop and another on a runner.
    ///
    /// Recorded as it stands, the command gains a shell on every save and
    /// restore and never comes back. So it is refused instead, and the caller
    /// counts it against the save rather than writing it down wrongly.
    #[test]
    fn a_wrapper_a_shell_never_got_out_of_the_way_of_is_refused() {
        let ran = to_spawn(&argv(&["claude", "--continue"]));
        let buried = vec![
            "/bin/bash".to_string(),
            "-lc".to_string(),
            // Quoted the way the node's own joiner would leave it.
            format!("sh -mc '{}' sh claude --continue", KEEP_THE_SHELL),
        ];
        assert!(
            still_wrapped(&buried),
            "a wrapper inside a snippet is still a wrapper: {buried:?}"
        );

        // And the shapes that are fine are not caught by it.
        assert!(!still_wrapped(&argv(&["claude", "--continue"])));
        assert!(!still_wrapped(&argv(&["/usr/bin/zsh", "-l"])));
        assert!(!still_wrapped(&[]));

        // The distinction the caller turns on, and the reason it asks this of
        // the *answer* rather than of the raw foreground. The plain wrapper
        // carries the marker and is read straight through, which is what a
        // machine reports in the moment between a spawn and the command
        // reaching the front of the terminal; asked of the raw form, a save
        // taken straight after a restore was refused for describing exactly
        // what it had just started.
        assert!(still_wrapped(&ran), "the spawn form does carry the marker");
        let read_through = resumed(&ran[1..]);
        assert_eq!(read_through, argv(&["claude", "--continue"]));
        assert!(
            !still_wrapped(&read_through),
            "so the answer is clean and the caller keeps it"
        );

        // Where nothing could read it, the answer still carries the marker and
        // the caller refuses it.
        assert!(still_wrapped(&resumed(&buried)));
    }

    /// The wrapper exists so that quitting the program leaves you where you
    /// were rather than ending the session.
    #[test]
    fn a_restored_command_keeps_the_shell_behind_it() {
        let spawn = to_spawn(&argv(&["claude", "--continue"]));
        assert_eq!(spawn[0], "exec", "the login shell steps aside first");
        assert_eq!(spawn[1], "sh");
        assert_eq!(spawn[2], "-mc", "with job control, for the reason above");
        assert!(spawn[3].contains("exec"), "the shell is exec'd after it");
        assert_eq!(
            &spawn[4..],
            &argv(&["sh", "claude", "--continue"])[..],
            "$0 first, then the words themselves, so nothing needs quoting here"
        );

        assert!(
            to_spawn(&[]).is_empty(),
            "a prompt is already what an empty spawn command means"
        );
    }

    /// The line `mm checkpoint show` prints under each session is the line that
    /// runs, so it is built the way the node builds one: `to_spawn` joined with
    /// `shell::join`. Anything else printed would be a second opinion about
    /// what is about to happen, and the words that make the two disagree are
    /// exactly the ones somebody reads that row to check.
    ///
    /// `SHELL` is a program that exits, since the wrapper ends by exec'ing a
    /// login shell and this is not a terminal.
    #[test]
    fn the_line_that_is_printed_is_the_line_that_runs() {
        let words = argv(&["printf", "%s|", "a b", r#"c'd"#, "--flag=x y"]);
        let line = crate::shell::join(&to_spawn(&words));
        let ran = std::process::Command::new("sh")
            .arg("-lc")
            .arg(&line)
            .env("SHELL", "/bin/true")
            .output()
            .expect("a shell to run the line with");
        assert_eq!(
            String::from_utf8_lossy(&ran.stdout),
            "a b|c'd|--flag=x y|",
            "the words come back as they were, out of {line}"
        );
    }

    #[test]
    fn the_checkpoint_file_round_trips() {
        let checkpoint = Checkpoint {
            taken: 1_700_000_000,
            sessions: vec![
                Kept {
                    host: "dev.box.ray".into(),
                    name: "manymux".into(),
                    cwd: "/home/dario/rayfish/manymux".into(),
                    group: Some("rayfish".into()),
                    command: argv(&["claude", "--continue"]),
                },
                Kept {
                    host: "local".into(),
                    name: "zsh-2".into(),
                    cwd: "/tmp".into(),
                    group: None,
                    command: Vec::new(),
                },
            ],
        };

        let text = toml::to_string_pretty(&checkpoint).unwrap();
        let back: Checkpoint = toml::from_str(&text).unwrap();
        assert_eq!(back.taken, checkpoint.taken);
        assert_eq!(back.sessions, checkpoint.sessions);

        // An empty file is a checkpoint with nothing in it rather than an
        // error, the way an absent one is.
        assert!(toml::from_str::<Checkpoint>("").unwrap().is_empty());
    }
}
