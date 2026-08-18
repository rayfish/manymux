//! Reaching a machine without forking anything.
//!
//! [`manymux::ssh`] runs the `ssh` binary and hands its pipes to the client. An
//! app has no binary to run, so what stands in its place is a connection made
//! in this process, and the only thing above it that changes is where the two
//! stream halves come from: [`manymux::client::Stream::from_halves`] takes them
//! and everything from the framing upwards is the library's, unchanged.
//!
//! What does *not* come for free is the ladder. `Stream::from_halves` leaves a
//! stream with no way back to a machine, so the retry built into
//! `Stream::request` never fires and climbing [`PROGRAMS`] is this module's job.
//! It is climbed with `Request::List`, which is the question a session list
//! wants answered anyway, so looking for `mm` costs no round trip of its own.
//!
//! Nothing here installs anything. `client::Consent` is never constructed, and
//! a machine at the end of the ladder is reported rather than offered a copy.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use manymux::client::{PROGRAMS, Stream};
use manymux::lock::held;
use manymux::proto::{Request, Response, SessionInfo};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::sync::watch;

/// A way to run one command on a machine and talk to it.
///
/// The two implementations are an ssh channel and a child process, and the
/// second exists so the ladder can be tested against a real shell's exit status
/// rather than against something's idea of one.
pub trait Exec {
    fn open(&self, command: &str) -> impl Future<Output = Result<Remote>> + Send;
}

/// One command running on the far end.
///
/// The status and the complaint are held apart from the byte halves because
/// [`Stream::from_halves`] takes those halves and gives nothing back: by the
/// time a rung has failed, the only things left to ask are how it ended and
/// what it said on the way out.
pub struct Remote {
    pub reader: Box<dyn AsyncRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub watch: Watch,
}

/// What became of a command, once there is a reason to care.
#[derive(Clone)]
pub struct Watch {
    status: watch::Receiver<Option<i32>>,
    said: Arc<Mutex<Vec<u8>>>,
}

impl Watch {
    /// The exit status, waited for rather than polled.
    ///
    /// A rung that had no `mm` is at EOF by the time anybody asks, so the wait
    /// is already over; polling would race the far end into answering `None`
    /// and read a missing `mm` as a machine that broke.
    pub async fn ended(&mut self) -> Option<i32> {
        if self.status.borrow().is_none() {
            let _ = self.status.changed().await;
        }
        *self.status.borrow()
    }

    /// What the far end complained about, if anything.
    pub fn said(&self) -> String {
        String::from_utf8_lossy(&held(&self.said))
            .trim()
            .to_string()
    }
}

/// The writing half of a [`Watch`], held by whoever is pumping the command.
///
/// Dropping one without saying how the command ended answers `None`, which is
/// what a connection that went away in the middle of a rung amounts to.
pub struct Ending {
    status: watch::Sender<Option<i32>>,
    said: Arc<Mutex<Vec<u8>>>,
}

impl Ending {
    pub fn new() -> (Self, Watch) {
        let (status, receiver) = watch::channel(None);
        let said = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                status,
                said: Arc::clone(&said),
            },
            Watch {
                status: receiver,
                said,
            },
        )
    }

    /// Something the far end wrote to its stderr.
    pub fn said(&self, bytes: &[u8]) {
        held(&self.said).extend_from_slice(bytes);
    }

    /// How the command ended.
    pub fn ended(&self, code: Option<i32>) {
        let _ = self.status.send(code);
    }
}

/// A machine reached, and the listing that reaching it produced.
pub struct Reached {
    pub stream: Stream,
    pub sessions: Vec<SessionInfo>,
    /// Which of [`PROGRAMS`] answered. Worth keeping: every later command to
    /// this machine should start at the rung that worked.
    pub program: &'static str,
}

/// Climb [`PROGRAMS`] until one of them answers the protocol.
pub async fn reach<E: Exec>(exec: &E) -> Result<Reached> {
    let mut complaint = String::new();

    for program in PROGRAMS {
        let remote = exec.open(&format!("{program} agent")).await?;
        let mut watch = remote.watch;
        let mut stream = Stream::from_halves(remote.reader, remote.writer);

        match stream.request(&Request::List).await {
            Ok(Response::Sessions(sessions)) => {
                return Ok(Reached {
                    stream,
                    sessions,
                    program,
                });
            }
            Ok(Response::Error(said)) => bail!(said),
            Ok(other) => bail!("expected a listing, got {other:?}"),
            Err(failed) => {
                // 127 is a shell saying it could not find the program, which is
                // the one failure that means try the next spelling. Anything
                // else is a machine that answered badly and must not be asked
                // again a different way.
                if watch.ended().await != Some(NOT_FOUND) {
                    let said = watch.said();
                    return Err(failed.context(said));
                }
                // The `mm: command not found` behind this is the probe working.
                // Held rather than printed: it is on the way to being the error
                // for a machine with no `mm` at all, and noise on every command
                // to a machine that simply keeps its copy at home.
                complaint = watch.said();
            }
        }
    }

    bail!(
        "no `mm` on it: tried {}{}",
        PROGRAMS.join(" and "),
        if complaint.is_empty() {
            String::new()
        } else {
            format!(" ({complaint})")
        }
    )
}

/// What a shell exits with for a command it could not find. The same constant
/// the library keeps privately, and for the same reason: it is the only sign
/// the far end gives that it has no `mm` on it.
const NOT_FOUND: i32 = 127;

impl Remote {
    /// Take over a command built by the caller and run it as a child process,
    /// for the tests and for nothing else.
    ///
    /// An app never takes this path: it has no processes to spawn, which is the
    /// whole reason this module exists. It is here so that what the ladder
    /// reads is a real shell's 127 rather than something's idea of one, which
    /// is also why the caller builds the command: what the far end resolves
    /// `mm` against is the thing under test.
    pub async fn spawn(mut command: tokio::process::Command) -> Result<Self> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;

        let reader = child.stdout.take().expect("stdout was piped");
        let writer = child.stdin.take().expect("stdin was piped");
        let mut said = child.stderr.take().expect("stderr was piped");

        let (ending, watch) = Ending::new();
        tokio::spawn(async move {
            let mut buffer = Vec::new();
            let _ = said.read_to_end(&mut buffer).await;
            ending.said(&buffer);
            ending.ended(child.wait().await.ok().and_then(|status| status.code()));
        });

        Ok(Self {
            reader: Box::new(reader),
            writer: Box::new(writer),
            watch,
        })
    }
}
