//! Reaching another machine, by running `mm agent` there over ssh.
//!
//! manymux does not do networking. You already have a way to reach your machines
//! (plain ssh, a jump host, tailscale, rayfish), and it is configured in
//! `~/.ssh/config` where it belongs. A "host" here is whatever ssh means by
//! that name, so anything ssh can reach, manymux can manage.
//!
//! This also settles authorization: sshd decides who gets in, using whatever
//! the admin configured. manymux keeps no allowlist of its own, so there is no
//! second ACL to drift out of sync with the real one, and no way for manymux to
//! grant access ssh would refuse.

use std::path::PathBuf;
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::process::{Child, Command};

/// How long a shared connection lingers after the last command using it.
///
/// This is what makes `mm ls` across five machines fast: the first command
/// pays for the handshake, the rest reuse it. Long enough to cover a working
/// session, short enough that a laptop that moved network is not holding a dead
/// master open.
const PERSIST: &str = "5m";

/// How ssh is made to notice a connection that died without closing.
///
/// A lid closed on a wifi hop kills the connection and closes nothing. Without
/// these, ssh sits on a socket to nowhere for as long as the process lives:
/// the kernel's own TCP keepalive is two hours away, and `ConnectTimeout` is
/// about reaching a machine rather than staying reached. That leaves the pipes
/// carrying the protocol open and silent, which is the one thing a reader
/// cannot tell from a session that has nothing to say.
///
/// Sharing connections makes it worse rather than better, and is why this is
/// not left to the protocol's own deadline. A master outlives the command that
/// made it, so a master whose network went takes every later command down with
/// it: `ControlMaster=auto` multiplexes onto it, and connecting to a control
/// socket has no timeout to give up on. One dead master is `mm ls`, `mm new`
/// and tab completion all hanging until somebody works out what a control
/// socket is.
///
/// Three tries at fifteen seconds is the same three-quarters of a minute the
/// protocol gives a silent peer ([`crate::proto::SILENT_FOR`]), so a client
/// that ssh has not yet given up on reaches the same conclusion at about the
/// same time by itself.
const ALIVE_EVERY: &str = "15";
const ALIVE_TRIES: &str = "3";

/// A running `ssh <host> mm agent`, and its pipes.
pub struct Agent {
    pub child: Child,
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
    /// Piped by [`agent`] and left for the caller to deal with, `None` from
    /// [`agent_quietly`], which throws it away at the source.
    pub stderr: Option<tokio::process::ChildStderr>,
}

/// Start `program agent` on `host` and hand back its pipes.
///
/// `host` is an ssh destination, so `gpu-box`, `dario@gpu-box` and any `Host`
/// alias from your ssh config all work, along with whatever `ProxyCommand` or
/// `ProxyJump` that alias carries. `program` is how `mm` is named over there;
/// see [`crate::client::PROGRAMS`] for why there is more than one answer.
pub fn agent(host: &str, program: &str) -> Result<Agent> {
    let mut command = command(host);
    // Piped rather than inherited, though ssh's own errors (host key,
    // permission denied) do have to reach the user in the end. The first
    // program tried is a probe, and on a machine whose `mm` is in a home
    // directory it makes the remote shell say so on this pipe; inheriting
    // would put that line on the terminal of every command that reaches such a
    // machine. Whether any of it is worth showing is decided by whoever holds
    // the ladder, in `client::Stream`.
    command.stderr(Stdio::piped());
    spawn(command, host, program)
}

/// Like [`agent`], but for a question nobody asked out loud.
///
/// Tab completion runs this on a keystroke, so ssh must not stop to ask about a
/// host key or a passphrase, and must not print over the line being typed. A
/// machine that cannot be reached without a conversation simply has no sessions
/// to offer, which is the right answer for a tab.
pub fn agent_quietly(host: &str, program: &str) -> Result<Agent> {
    let mut command = base(Tty::No);
    command
        // Fail rather than prompt, and give up on a machine that is not
        // answering instead of sitting on the default 2 minute connect timeout.
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=1")
        .arg(host)
        .stderr(Stdio::null());
    spawn(command, host, program)
}

fn spawn(mut command: Command, host: &str, program: &str) -> Result<Agent> {
    let mut child = command
        .arg("--")
        // Never a probe first: working out where `mm` is by asking the far end
        // would mean either sourcing profiles, whose output would corrupt the
        // protocol on stdout, or a round trip before every command. Naming one
        // and reading the exit status costs nothing when the guess is right.
        .arg(program)
        .arg("agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running ssh {host}"))?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take();
    Ok(Agent {
        child,
        stdin,
        stdout,
        stderr,
    })
}

/// Connect once with the terminal attached, so ssh can ask its questions.
///
/// The protocol needs stdin, which leaves ssh no way to prompt for a host key
/// or a passphrase. Doing one interactive connection first gets those out of
/// the way and, because connections are shared, leaves a master open for the
/// commands that follow.
pub async fn greet(host: &str) -> Result<()> {
    let status = command(host)
        .arg("--")
        .arg("true")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("running ssh {host}"))?;
    if !status.success() {
        bail!("ssh {host} failed ({status})");
    }
    Ok(())
}

/// Put `mm` on a machine you can already ssh into, by running the published
/// installer there.
///
/// With a terminal, unlike everything else here. The installer takes root when
/// it can, because `/usr/local/bin` is the only directory on the PATH sshd
/// hands a command and so the only place an install is reachable from another
/// machine. Without a tty, sudo has no way to ask, and every machine with a
/// password on it would quietly take the per-user fallback instead.
pub async fn install(host: &str) -> Result<()> {
    let status = base(Tty::Wanted)
        .arg(host)
        .arg("--")
        // One string, because it is a pipeline for the remote shell to read
        // rather than a program and its arguments.
        .arg(format!("curl -fsSL {INSTALLER} | sh"))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("running ssh {host}"))?;
    if !status.success() {
        bail!("installing mm on {host} failed ({status})");
    }
    Ok(())
}

/// The installer, the same one-liner the README gives.
const INSTALLER: &str = "https://raw.githubusercontent.com/rayfish/manymux/master/install.sh";

/// An ssh invocation set up to share one connection per host.
///
/// `MM_SSH` replaces the program, for anyone whose ssh lives somewhere
/// unusual or who wraps it in a script. Tests use it to stand in for a second
/// machine without needing a real sshd.
pub fn command(host: &str) -> Command {
    let mut command = base(Tty::No);
    command.arg(host);
    command
}

/// Whether the remote command gets a terminal. Only the installer wants one,
/// and only so sudo can ask for a password.
enum Tty {
    No,
    Wanted,
}

/// The options every invocation shares, with no destination yet. ssh reads
/// everything after the destination as the remote command, so a caller wanting
/// options of its own has to add them before naming the host.
fn base(tty: Tty) -> Command {
    let program = std::env::var("MM_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut command = Command::new(program);
    command
        .arg(match tty {
            // No PTY: this carries a framed protocol, and a PTY would mangle
            // it. `-T` also wins over `-t` whichever order they arrive in, so
            // this is a choice rather than a default to override later.
            Tty::No => "-T",
            Tty::Wanted => "-t",
        })
        // Share one connection per destination, so the second command to a
        // host skips the handshake entirely.
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg(format!("ControlPath={}", control_path().display()))
        .arg("-o")
        .arg(format!("ControlPersist={PERSIST}"))
        // Notice a connection that went, rather than holding its pipes open
        // and silent forever. On the master this is what keeps one dead
        // connection from hanging every command that follows it.
        .arg("-o")
        .arg(format!("ServerAliveInterval={ALIVE_EVERY}"))
        .arg("-o")
        .arg(format!("ServerAliveCountMax={ALIVE_TRIES}"));
    command
}

/// Where ssh keeps its shared-connection sockets.
///
/// Under the user's runtime directory, which is already private, and keyed by
/// destination so two hosts never share a master. `%C` is ssh's own hash of
/// (host, port, user, proxy), which keeps the path short enough for the ~104
/// byte limit on socket names.
fn control_path() -> PathBuf {
    // ssh binds the master socket but will not create the directory holding it,
    // and on a machine that only ever talks to other machines no node has run to
    // create it either, so the first command would die with ENOENT. Best effort:
    // if the directory cannot be made, ssh reports it better than a guess here.
    let dir = crate::config::runtime_dir();
    let _ = crate::config::ensure_runtime_dir();
    dir.join("ssh-%C")
}
