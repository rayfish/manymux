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

/// A running `ssh <host> mm agent`, and its pipes.
pub struct Agent {
    pub child: Child,
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
}

/// Start `mm agent` on `host` and hand back its pipes.
///
/// `host` is an ssh destination, so `gpu-box`, `dario@gpu-box` and any `Host`
/// alias from your ssh config all work, along with whatever `ProxyCommand` or
/// `ProxyJump` that alias carries.
pub fn agent(host: &str) -> Result<Agent> {
    let mut child = command(host)
        .arg("--")
        // Plain `mm`, found on the PATH a non-interactive ssh gets, which is
        // why the installer puts it in /usr/local/bin. Probing for it here
        // instead would mean either sourcing profiles, whose output would
        // corrupt the protocol on stdout, or guessing at directories.
        .arg("mm")
        .arg("agent")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // Let ssh's own errors (host key, permission denied) reach the user's
        // terminal rather than vanishing.
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("running ssh {host}"))?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");
    Ok(Agent {
        child,
        stdin,
        stdout,
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

/// An ssh invocation set up to share one connection per host.
///
/// `MM_SSH` replaces the program, for anyone whose ssh lives somewhere
/// unusual or who wraps it in a script. Tests use it to stand in for a second
/// machine without needing a real sshd.
pub fn command(host: &str) -> Command {
    let program = std::env::var("MM_SSH").unwrap_or_else(|_| "ssh".to_string());
    let mut command = Command::new(program);
    command
        // No PTY: this carries a framed protocol, and a PTY would mangle it.
        .arg("-T")
        // Share one connection per destination, so the second command to a
        // host skips the handshake entirely.
        .arg("-o")
        .arg("ControlMaster=auto")
        .arg("-o")
        .arg(format!("ControlPath={}", control_path().display()))
        .arg("-o")
        .arg(format!("ControlPersist={PERSIST}"))
        .arg(host);
    command
}

/// Where ssh keeps its shared-connection sockets.
///
/// Under the user's runtime directory, which is already private, and keyed by
/// destination so two hosts never share a master. `%C` is ssh's own hash of
/// (host, port, user, proxy), which keeps the path short enough for the ~104
/// byte limit on socket names.
fn control_path() -> PathBuf {
    let dir = crate::config::runtime_dir();
    // ssh binds the master socket but will not create the directory holding it,
    // and on a machine that only ever talks to other machines no node has run to
    // create it either, so the first command would die with ENOENT. Best effort:
    // if the directory cannot be made, ssh reports it better than a guess here.
    let _ = crate::config::ensure_private_dir(&dir);
    dir.join("ssh-%C")
}
