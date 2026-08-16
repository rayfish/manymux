//! Tab completion that knows which sessions exist.
//!
//! A generated script can only offer what was true when it was written, which
//! for `mm attach` is nothing at all. So the script is a stub that asks this
//! binary on every tab, and the answers come from the same `Request::List` the
//! rest of the CLI uses.
//!
//! Two rules shape everything here, because this code runs on a keystroke:
//!
//! - A tab never starts a node. No session listed is a real answer.
//! - A tab never waits on ssh unless asked to. A bare `mm a <TAB>` offers this
//!   machine's sessions and `host/` as a doorway; only once the word names a
//!   machine does one ssh go out, on a budget.

use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::CommandFactory as _;
use clap_complete::engine::{
    ArgValueCompleter, CompletionCandidate, PathCompleter, ValueCompleter,
};
use clap_complete::{CompleteEnv, Shell};

use manymux::client::Stream;
use manymux::client::groups::Groups;
use manymux::hosts::{Hosts, LOCAL, is_this_machine};
use manymux::proto::{Request, Response, SessionInfo};

use crate::Cli;

/// The environment variable the stub sets to ask for completions.
const VAR: &str = "COMPLETE";

/// How long the local node gets to answer. It is a unix socket and a running
/// process; longer than this means something is wrong, and a tab is not the
/// place to find out.
const LOCAL_BUDGET: Duration = Duration::from_millis(300);

/// How long another machine gets. Enough for a warm shared connection to
/// answer, short enough that a machine that is asleep costs you a pause rather
/// than a hang.
const REMOTE_BUDGET: Duration = Duration::from_millis(1500);

/// Set to let a bare tab reach every watched machine, rather than only offering
/// `host/` and going out when you name one.
const REMOTE_VAR: &str = "MM_COMPLETE_REMOTE";

/// Answer a completion request and exit, or return and let the CLI run.
///
/// Called before the runtime starts and before the arguments are parsed: a
/// completion request is not a command line this parser would accept, and the
/// completers below want a runtime of their own.
pub fn intercept() {
    CompleteEnv::with_factory(Cli::command).var(VAR).complete();
}

/// Write the shell's side of this: a stub that calls back here.
pub fn registration(shell: Shell, out: &mut dyn Write) -> Result<()> {
    let name = shell.to_string();
    let shells = clap_complete::env::Shells::builtins();
    let Some(completer) = shells.completer(&name) else {
        anyhow::bail!("no completion support for {name}");
    };
    // The completer is plain `mm` on the PATH, for the same reason the agent is
    // (see `ssh::agent`): the installed script should not name a path that a
    // later move or update invalidates.
    completer.write_registration(VAR, "mm", "mm", "mm", out)?;
    if shell == Shell::Zsh {
        out.write_all(ZSH_AUTOLOAD_SHIM.as_bytes())?;
    }
    Ok(())
}

/// zsh's registration is written to be sourced from `.zshrc`, where `compdef`
/// runs long before the first tab. We install onto `fpath` instead, where the
/// file is autoloaded *by* the first tab, and `compdef` there only takes effect
/// from the next one. So call what it just registered, read back out of
/// `_comps` rather than by naming a function that belongs to clap_complete.
/// The file has to work both ways, since sourcing it from `.zshrc` is the other
/// way people set this up. `CURRENT` is only set while a completion is running,
/// which is what tells the two apart: sourced, this does nothing and `compdef`
/// alone is enough.
const ZSH_AUTOLOAD_SHIM: &str = r#"
# Autoloaded from fpath, `compdef` above applies from the next completion
# onwards, so run what it registered rather than letting this tab come back
# empty. Sourced from .zshrc instead, there is no completion to answer and
# `CURRENT` is unset, so this stays out of the way.
if (( ${+CURRENT} )); then
  local _mm_completer=${_comps[mm]}
  if [[ -n $_mm_completer && $_mm_completer != $0 ]]; then
    $_mm_completer "$@"
  fi
fi
"#;

/// Sessions, for `attach`, `kill` and `rename`.
pub fn targets() -> ArgValueCompleter {
    ArgValueCompleter::new(target_candidates)
}

/// Machines being watched, plus this one, for `ls`.
pub fn hosts_or_local() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let mut names = vec![LOCAL.to_string()];
        names.extend(watched());
        candidates(prefixed(current, names))
    })
}

/// Group names, for `mm group`.
///
/// A local file read, so this starts no node, installs nothing and waits on no
/// ssh, which is the rule every completer here is held to.
pub fn group_names() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| candidates(prefixed(current, groups())))
}

fn groups() -> Vec<String> {
    Groups::load().map(|held| held.names()).unwrap_or_default()
}

/// Machines being watched, for `rm`. Not this one: it is not on the list.
pub fn watched_hosts() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| candidates(prefixed(current, watched())))
}

/// Setting names, for `config`. A fixed list: nothing here asks a node
/// anything, which is what keeps a tab from starting one.
pub fn settings() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let names: Vec<String> = manymux::settings::KEYS
            .iter()
            .map(|k| k.to_string())
            .collect();
        candidates(prefixed(current, names))
    })
}

/// What a setting can be set to, which depends on which setting is being set:
/// `notify` is on or off, `screen` is a screen. Read off the line being
/// completed the same way the socket is.
pub fn setting_values() -> ArgValueCompleter {
    ArgValueCompleter::new(|current: &OsStr| {
        let key = typed_setting(std::env::args_os()).unwrap_or_default();
        let values: Vec<String> = manymux::settings::values(&key)
            .iter()
            .map(|value| value.to_string())
            .collect();
        candidates(prefixed(current, values))
    })
}

/// The setting a `mm config <key> <tab>` is setting, if there is one.
fn typed_setting<I>(args: I) -> Option<String>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter().skip_while(|arg| arg != "--").skip(1);
    while let Some(arg) = args.next() {
        if arg == "config" {
            return args.next()?.to_str().map(str::to_string);
        }
    }
    None
}

/// `mm new [host] [command...]`: a machine or a program first, then arguments.
pub fn new_args() -> ArgValueCompleter {
    ArgValueCompleter::new(NewArgs)
}

struct NewArgs;

impl ValueCompleter for NewArgs {
    fn complete(&self, current: &OsStr) -> Vec<CompletionCandidate> {
        self.complete_at(0, current)
    }

    fn complete_at(&self, index: usize, current: &OsStr) -> Vec<CompletionCandidate> {
        let mut found = if index == 0 {
            candidates(prefixed(current, watched()))
        } else {
            Vec::new()
        };
        // Everything after the first word is arguments to a program, and the
        // first word is usually a program too, so keep the filename completion
        // the generated script used to fall back to.
        found.extend(PathCompleter::any().complete(current));
        found
    }
}

fn target_candidates(current: &OsStr) -> Vec<CompletionCandidate> {
    // A session name is a word someone typed at a shell; if this one is not
    // even text, there is nothing to match it against.
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let socket = socket();

    // `host/`: they have said which machine, so go and ask it, and only it.
    if let Some((host, _)) = current.split_once('/') {
        if host.is_empty() || !(is_this_machine(host) || watched().iter().any(|h| h == host)) {
            return Vec::new();
        }
        let found = sessions(&socket, host, REMOTE_BUDGET);
        return described(
            found
                .into_iter()
                .map(|session| (format!("{host}/{}", session.name), session.title))
                .filter(|(target, _)| target.starts_with(current)),
        );
    }

    let here = sessions(&socket, LOCAL, LOCAL_BUDGET);
    let hosts = watched();
    if remote_wanted() {
        let mut found: Vec<(String, String)> = here
            .into_iter()
            .map(|session| (session.name, session.title))
            .collect();
        for host in &hosts {
            found.extend(
                sessions(&socket, host, REMOTE_BUDGET)
                    .into_iter()
                    .map(|session| (format!("{host}/{}", session.name), session.title)),
            );
        }
        return described(
            found
                .into_iter()
                .filter(|(target, _)| target.starts_with(current)),
        );
    }

    candidates(plan(current, &here, &hosts))
}

/// What a bare word completes to: sessions here, and a door to each machine.
///
/// Kept free of I/O so the policy can be tested without a socket in sight.
fn plan(current: &str, here: &[SessionInfo], hosts: &[String]) -> Vec<String> {
    let names = here.iter().map(|session| session.name.clone());
    // A trailing slash, so accepting one goes straight on to naming a session
    // there rather than looking like a finished answer.
    let doors = hosts.iter().map(|host| format!("{host}/"));
    prefixed_str(current, names.chain(doors))
}

fn prefixed<I>(current: &OsStr, values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    match current.to_str() {
        Some(current) => prefixed_str(current, values),
        None => Vec::new(),
    }
}

fn prefixed_str<I>(current: &str, values: I) -> Vec<String>
where
    I: IntoIterator<Item = String>,
{
    values
        .into_iter()
        .filter(|value| value.starts_with(current))
        .collect()
}

fn candidates(values: Vec<String>) -> Vec<CompletionCandidate> {
    values.into_iter().map(CompletionCandidate::new).collect()
}

/// Candidates carrying what the session calls itself, which is what tells two
/// `shell` sessions apart in the list a shell shows.
fn described<I>(values: I) -> Vec<CompletionCandidate>
where
    I: IntoIterator<Item = (String, String)>,
{
    values
        .into_iter()
        .map(|(target, title)| CompletionCandidate::new(target).help(Some(title.into())))
        .collect()
}

fn watched() -> Vec<String> {
    Hosts::load().map(|hosts| hosts.names()).unwrap_or_default()
}

fn remote_wanted() -> bool {
    matches!(
        std::env::var(REMOTE_VAR).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// Sessions on one machine, or none if it cannot say within `budget`.
///
/// Every failure is the same answer here: nothing to offer. A tab is not the
/// place to explain that a machine is asleep.
fn sessions(socket: &Path, host: &str, budget: Duration) -> Vec<SessionInfo> {
    blocking(budget, async {
        // No node here means no sessions here. Starting one because someone
        // pressed tab would be rude, and slow.
        if is_this_machine(host) {
            if !socket.exists() {
                return Vec::new();
            }
            return ask(Stream::local(socket).await).await;
        }
        ask(Stream::over_ssh_quietly(host).await).await
    })
    .unwrap_or_default()
}

async fn ask(stream: Result<Stream>) -> Vec<SessionInfo> {
    let Ok(mut stream) = stream else {
        return Vec::new();
    };
    match stream.request(&Request::List).await {
        Ok(Response::Sessions(sessions)) => sessions,
        _ => Vec::new(),
    }
}

/// Run one async errand to completion, or give up on it.
///
/// Completion happens before the CLI's runtime exists (see `main`), so there is
/// no runtime to be inside of here, and building one is allowed. Dropping the
/// future on a timeout takes the ssh child with it, which is `kill_on_drop`.
fn blocking<T>(budget: Duration, work: impl Future<Output = T>) -> Option<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async { tokio::time::timeout(budget, work).await.ok() })
}

/// The socket to ask, honouring a `--socket` on the line being completed.
///
/// A completer is handed the current word and nothing else, but the whole line
/// is sitting in this process's own arguments, after the `--` that separates
/// our invocation from theirs:
///
/// ```text
/// mm -- mm --socket /tmp/x.sock a bui
/// ```
fn socket() -> PathBuf {
    typed_socket(std::env::args_os()).unwrap_or_else(manymux::config::socket)
}

fn typed_socket<I>(args: I) -> Option<PathBuf>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = args.into_iter().skip_while(|arg| arg != "--").skip(1);
    while let Some(arg) = args.next() {
        if arg == "--socket" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--socket=")) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use super::*;
    use manymux::proto::Size;

    fn session(name: &str) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            title: name.to_string(),
            command: "zsh".to_string(),
            pid: 1,
            size: Size { rows: 24, cols: 80 },
            attached: 0,
            idle: 0,
            bells: 0,
            started: SystemTime::now(),
        }
    }

    fn hosts() -> Vec<String> {
        vec!["gpu-box".to_string(), "laptop".to_string()]
    }

    #[test]
    fn a_bare_tab_offers_sessions_here_and_a_door_to_each_machine() {
        let here = [session("api"), session("build")];
        assert_eq!(
            plan("", &here, &hosts()),
            ["api", "build", "gpu-box/", "laptop/"]
        );
    }

    #[test]
    fn typing_narrows_to_what_still_matches() {
        let here = [session("api"), session("build")];
        assert_eq!(plan("bu", &here, &hosts()), ["build"]);
        assert_eq!(plan("gpu-", &here, &hosts()), ["gpu-box/"]);
        assert!(plan("nothing", &here, &hosts()).is_empty());
    }

    #[test]
    fn a_machine_with_no_sessions_still_opens_its_door() {
        assert_eq!(plan("", &[], &hosts()), ["gpu-box/", "laptop/"]);
    }

    #[test]
    fn the_socket_comes_from_the_line_being_completed() {
        let line =
            |args: &[&str]| typed_socket(args.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(
            line(&["mm", "--", "mm", "--socket", "/tmp/x.sock", "a", ""]),
            Some(PathBuf::from("/tmp/x.sock"))
        );
        assert_eq!(
            line(&["mm", "--", "mm", "--socket=/tmp/x.sock", "a", ""]),
            Some(PathBuf::from("/tmp/x.sock"))
        );
        assert_eq!(line(&["mm", "--", "mm", "a", ""]), None);
        // Ours, not theirs: everything before the `--` is how the stub called
        // us, and says nothing about the line being typed.
        assert_eq!(
            line(&["mm", "--socket", "/ours", "--", "mm", "a", ""]),
            None
        );
    }

    #[test]
    fn the_zsh_script_completes_on_the_first_tab() {
        let mut written = Vec::new();
        registration(Shell::Zsh, &mut written).unwrap();
        let script = String::from_utf8(written).unwrap();
        assert!(script.starts_with("#compdef mm"));
        assert!(script.contains("COMPLETE"));
        // The shim, without which the first tab after a shell starts is eaten.
        assert!(script.contains("_comps[mm]"));
        // Guarded, so sourcing the file from .zshrc does not run it as though a
        // completion were in progress.
        assert!(script.contains("${+CURRENT}"));
    }

    #[test]
    fn every_installable_shell_gets_a_script_that_asks_the_binary() {
        for shell in [Shell::Bash, Shell::Fish, Shell::Elvish] {
            let mut written = Vec::new();
            registration(shell, &mut written).unwrap();
            let script = String::from_utf8(written).unwrap();
            assert!(script.contains("COMPLETE"), "{shell}: {script}");
            assert!(script.contains("mm"), "{shell}: {script}");
        }
    }
}
