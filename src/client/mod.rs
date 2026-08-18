//! The client half.
//!
//! Everything here is transport-agnostic and terminal-agnostic: a [`Stream`] is
//! a Unix-socket connection to the node on this machine, or the pipes of an
//! `ssh <host> mm agent` running on another one. An [`Attached`] session
//! hands out bytes rather than driving a terminal. The CLI is one consumer; a
//! mobile app rendering the session with its own terminal widget is another.

pub mod attach;
pub mod checkpoint;
pub mod groups;
pub mod picker;
pub mod screen;
pub mod scroll;
pub mod status;
pub mod switch;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::process::{Child, ChildStderr};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::proto::{
    self, FindRequest, FrameReader, HostedEvent, PasteInfo, Request, Response, Size, ViewRequest,
    tag,
};

type Reader = FrameReader<Box<dyn AsyncRead + Unpin + Send>>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

/// The ssh process carrying a stream to another machine. Dropping it hangs up,
/// which is why it travels with the stream rather than being left behind.
struct Carrier {
    child: Child,
    /// Say the word and ssh's stderr is printed, and keeps being printed.
    /// Dropped instead, it is thrown away. See [`relay`].
    voice: Option<oneshot::Sender<()>>,
    /// The relay itself, for waiting on when the word has been said and the
    /// process is about to end.
    spoken: Option<JoinHandle<()>>,
}

impl Carrier {
    fn new(child: Child, stderr: Option<ChildStderr>) -> Self {
        let (voice, spoken) = match stderr {
            Some(stderr) => {
                let (voice, spoken) = relay(stderr);
                (Some(voice), Some(spoken))
            }
            None => (None, None),
        };
        Self {
            child,
            voice,
            spoken,
        }
    }
}

/// Read ssh's stderr, holding it back until this way onto the machine has
/// proved itself.
///
/// The first of [`PROGRAMS`] fails on every machine whose `mm` is in a home
/// directory, and what it leaves on stderr is the remote shell's `mm: command
/// not found`. That is the probe working rather than anything wrong, so a
/// person watching should never see it, least of all on a terminal that is
/// about to go into raw mode and repaint a session over it.
///
/// What is worth showing is everything else ssh says, and the same pipe
/// carries it: no route to the host, a refused key, a connection dropped an
/// hour into a session. So none of it is discarded on a guess about its
/// contents. Sending on the returned channel prints what has arrived so far
/// and lets the rest through as it comes; dropping it throws the lot away,
/// which is what the attempt that answered 127 has earned.
fn relay(mut stderr: ChildStderr) -> (oneshot::Sender<()>, JoinHandle<()>) {
    let (voice, mut asked) = oneshot::channel();
    let relaying = tokio::spawn(async move {
        let mut held = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            tokio::select! {
                answer = &mut asked => {
                    // The other end is gone: so is any reason to keep this.
                    if answer.is_err() {
                        return;
                    }
                    break;
                }
                read = stderr.read(&mut buf) => match read {
                    Ok(0) | Err(_) => {
                        // ssh has said its piece and gone. Whether anyone wants
                        // to hear it is still open, so wait to be told.
                        if asked.await.is_ok() {
                            say(&held).await;
                        }
                        return;
                    }
                    Ok(n) => held.extend_from_slice(&buf[..n]),
                },
            }
        }
        say(&held).await;
        // Past here this is the connection carrying a session, and its troubles
        // are the user's as they happen.
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::stderr()).await;
    });
    (voice, relaying)
}

/// Put something on stderr and wait for it to actually be there.
///
/// The flush is the point. `tokio::io::stderr` hands its bytes to a blocking
/// thread and `write_all` returns as soon as they are queued, not when the
/// thread has run; only the flush waits for that. `mm` ends by calling
/// `process::exit` rather than by returning, so nothing drops the runtime and
/// waits for that thread on the way out, and an unflushed write is simply lost.
/// What got lost was ssh's account of a machine it could not reach, on the one
/// path where that account is the only one there is.
async fn say(text: &[u8]) {
    let mut stderr = tokio::io::stderr();
    let _ = stderr.write_all(text).await;
    let _ = stderr.flush().await;
}

/// The names `mm` might answer to on another machine, tried in order.
///
/// A bare `mm` is the one that should work: the installer puts it in
/// `/usr/local/bin`, which is on the PATH sshd hands a command. An install
/// without root lands in the home directory instead, and the shell behind
/// `ssh host mm agent` reads no profile that would put that on the PATH, so
/// such a machine has to be named outright or it looks like it has no `mm` at
/// all. The remote shell expands the `~` without reading anything.
pub const PROGRAMS: [&str; 2] = ["mm", "~/.local/bin/mm"];

/// What a shell exits with for a command it could not find, and the only sign
/// the far end gives that it has no `mm` on it.
const NOT_FOUND: i32 = 127;

/// Whether to put `mm` on a machine that turns out not to have it, asked of
/// whoever is running this. Returning false leaves the machine alone.
///
/// The question reaches a person only on the CLI; a library consumer that wants
/// nothing installed anywhere simply never supplies one.
pub type Consent = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// How to get back to a machine when the way in did not work.
struct Ssh {
    host: String,
    /// The quiet spawn for questions nobody asked, the loud one otherwise.
    open: fn(&str, &str) -> Result<crate::ssh::Agent>,
    consent: Option<Consent>,
    /// Which of [`PROGRAMS`] the open stream is using.
    program: usize,
    /// Whether the installer has already been run, so a machine that still has
    /// no `mm` afterwards is answered with an error rather than a second go.
    installed: bool,
}

/// One request/response exchange, which an `Attach` extends into a byte pipe.
pub struct Stream {
    read: Reader,
    write: Writer,
    carrier: Option<Carrier>,
    /// Set only on a stream over ssh, and only while it can still be reopened
    /// a different way.
    ssh: Option<Ssh>,
}

impl Stream {
    /// Connect to the node on this machine.
    pub async fn local(socket: &Path) -> Result<Self> {
        let stream = UnixStream::connect(socket).await.with_context(|| {
            format!(
                "connecting to {}: is `mm daemon` running?",
                socket.display()
            )
        })?;
        let (read, write) = stream.into_split();
        Ok(Self {
            read: FrameReader::new(Box::new(read)),
            write: Box::new(write),
            carrier: None,
            ssh: None,
        })
    }

    /// Reach another machine by running `mm agent` there over ssh.
    ///
    /// `host` is an ssh destination, so anything your ssh config can reach,
    /// this can reach, and sshd decides whether you are allowed in.
    ///
    /// A machine with no `mm` on it is offered one, if `consent` says so; see
    /// [`Consent`] and [`PROGRAMS`].
    pub async fn over_ssh(host: &str, consent: Option<Consent>) -> Result<Self> {
        Self::carried_by(host, crate::ssh::agent, consent)
    }

    /// Like [`Stream::over_ssh`], but ssh stays silent and does not prompt.
    ///
    /// For asking a question on someone's behalf, such as tab completion, where
    /// a password prompt or a host-key warning would land in the middle of the
    /// line they are typing. Nothing gets installed on a keystroke either.
    pub async fn over_ssh_quietly(host: &str) -> Result<Self> {
        Self::carried_by(host, crate::ssh::agent_quietly, None)
    }

    fn carried_by(
        host: &str,
        open: fn(&str, &str) -> Result<crate::ssh::Agent>,
        consent: Option<Consent>,
    ) -> Result<Self> {
        let ssh = Ssh {
            host: host.to_string(),
            open,
            consent,
            program: 0,
            installed: false,
        };
        let agent = (ssh.open)(&ssh.host, PROGRAMS[ssh.program])?;
        Ok(Self {
            read: FrameReader::new(Box::new(agent.stdout)),
            write: Box::new(agent.stdin),
            carrier: Some(Carrier::new(agent.child, agent.stderr)),
            ssh: Some(ssh),
        })
    }

    /// Wrap an already-open pair of stream halves. The escape hatch for tests
    /// and for transports this module doesn't know about.
    pub fn from_halves(read: Box<dyn AsyncRead + Unpin + Send>, write: Writer) -> Self {
        Self {
            read: FrameReader::new(read),
            write,
            carrier: None,
            ssh: None,
        }
    }

    /// Send a request and read its response.
    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        loop {
            // An ssh that has already given up leaves nothing to write to, and
            // whether it gave up because the machine has no `mm` is worth far
            // more than the broken pipe. Which of the two happens is a race
            // between the write and the far end's shell, so both lead here.
            let frame = match proto::write_msg(&mut self.write, tag::REQUEST, request).await {
                Ok(()) => self.read.next().await?,
                Err(e) => {
                    if self.reopen().await? {
                        continue;
                    }
                    return Err(e);
                }
            };
            let Some(frame) = frame else {
                // An ssh that failed to connect, or a remote with no `mm` on
                // it, leaves nothing on the pipe. Its exit status says far more
                // than "the stream ended", so go and look.
                if self.reopen().await? {
                    continue;
                }
                if let Some(carrier) = &mut self.carrier
                    && let Ok(Some(status)) = carrier.child.try_wait()
                {
                    bail!("ssh exited with {status} before answering");
                }
                bail!("the host closed the connection without responding");
            };
            // A frame came back, so this is a machine with an `mm` on it and
            // the probing is over. Anything ssh has to say from here is worth
            // hearing.
            self.speak_up();
            if frame.tag != tag::RESPONSE {
                bail!("expected a response, got tag {:#x}", frame.tag);
            }
            return proto::decode(&frame.body);
        }
    }

    /// Find another way onto a machine that turned out to have no `mm`, so the
    /// request that got nowhere can be sent again.
    ///
    /// Sending it twice is safe for exactly the reason this is reachable at
    /// all: 127 is the far end's shell saying it never ran anything, so the
    /// frame reached no node and nothing can happen a second time.
    ///
    /// Returns whether there is now a stream worth resending on. Anything other
    /// than a missing `mm` is left alone for the caller to report.
    async fn reopen(&mut self) -> Result<bool> {
        let Some(mut ssh) = self.ssh.take() else {
            self.speak_up();
            return Ok(false);
        };
        if !self.said_no_mm().await {
            // Not a missing `mm`, so ssh's own account of it is the only one
            // there is, and the caller is about to report a failure without it.
            self.speak_now().await;
            self.ssh = Some(ssh);
            return Ok(false);
        }

        if ssh.program + 1 < PROGRAMS.len() {
            ssh.program += 1;
        } else if !ssh.installed && ssh.consent.as_ref().is_some_and(|ask| ask(&ssh.host)) {
            crate::ssh::install(&ssh.host).await?;
            // Back to the top of the ladder: the installer takes root when it
            // can get it, and where it landed is its decision rather than ours.
            ssh.installed = true;
            ssh.program = 0;
        } else {
            bail!(
                "{}: no mm there, or none on the PATH a non-interactive ssh gets",
                ssh.host
            );
        }

        let agent = (ssh.open)(&ssh.host, PROGRAMS[ssh.program])?;
        self.read = FrameReader::new(Box::new(agent.stdout));
        self.write = Box::new(agent.stdin);
        // The old carrier goes here, taking the "command not found" it earned
        // with it.
        self.carrier = Some(Carrier::new(agent.child, agent.stderr));
        self.ssh = Some(ssh);
        Ok(true)
    }

    /// Let ssh be heard on this stream, now and for as long as it lasts.
    fn speak_up(&mut self) {
        if let Some(carrier) = &mut self.carrier
            && let Some(voice) = carrier.voice.take()
        {
            let _ = voice.send(());
        }
    }

    /// The same, waited on. Only for an ssh already reaped, and only where the
    /// caller is about to fail: a spawned relay dies with the runtime, and the
    /// runtime is one returned error away from stopping.
    async fn speak_now(&mut self) {
        self.speak_up();
        if let Some(carrier) = &mut self.carrier
            && let Some(relaying) = carrier.spoken.take()
        {
            let _ = relaying.await;
        }
    }

    /// Whether the ssh carrying this stream gave up because it found no `mm`.
    ///
    /// Waits rather than polling: its stdout is at EOF by the time this is
    /// asked, and a process whose only job was to relay that stdout has
    /// therefore exited, so the wait returns at once. `try_wait` would race the
    /// reap and read a machine with no `mm` as one that merely hung up.
    async fn said_no_mm(&mut self) -> bool {
        let Some(carrier) = &mut self.carrier else {
            return false;
        };
        matches!(carrier.child.wait().await, Ok(status) if status.code() == Some(NOT_FOUND))
    }

    /// Send a request whose answer is success or an error message.
    pub async fn call(&mut self, request: &Request) -> Result<Response> {
        match self.request(request).await? {
            Response::Error(message) => bail!(message),
            other => Ok(other),
        }
    }

    /// Read the next event on a stream that has been turned into a
    /// subscription with `Request::Events`. `None` is the end of the feed.
    ///
    /// A session server sends [`SessionEvent`](crate::proto::SessionEvent)s;
    /// the daemon's aggregated feed sends [`HostedEvent`](crate::proto::HostedEvent)s.
    pub async fn next_event<T: serde::de::DeserializeOwned>(&mut self) -> Result<Option<T>> {
        loop {
            let Some(frame) = self.read.next().await? else {
                return Ok(None);
            };
            // Skip anything else so a newer host can add frames without
            // breaking an older subscriber.
            if frame.tag == tag::EVENT {
                return Ok(Some(proto::decode(&frame.body)?));
            }
        }
    }

    /// Attach to a session, turning this stream into its byte pipe.
    ///
    /// `history` is how many lines of the session's scrollback to ask for,
    /// which arrive as [`Update::History`] before the repaint.
    /// `read_only` watches without typing. A host that does not answer that it
    /// understood the request is refused rather than attached to: the whole
    /// point is the promise, and an older node would hand back an ordinary
    /// session with a live keyboard.
    pub async fn attach(
        mut self,
        name: &str,
        size: Size,
        history: u32,
        read_only: bool,
    ) -> Result<Attached> {
        let request = Request::Attach {
            name: name.to_string(),
            size,
            history,
            read_only,
        };
        match self.request(&request).await? {
            Response::Attached {
                size,
                paste,
                scroll,
                rename,
                events,
                read_only: watching,
            } => {
                if read_only && !watching {
                    bail!(
                        "that machine runs a manymux too old to watch a session without \
                         typing into it; `mm update` there, or attach instead"
                    );
                }
                Ok(Attached {
                    read: self.read,
                    write: self.write,
                    carrier: self.carrier,
                    size,
                    paste,
                    scroll,
                    rename,
                    events,
                    read_only,
                })
            }
            Response::Error(message) => bail!(message),
            other => bail!("unexpected response to attach: {other:?}"),
        }
    }
}

/// An attached session. Split it to read output and send input at once.
pub struct Attached {
    read: Reader,
    write: Writer,
    carrier: Option<Carrier>,
    /// The size the session settled on, which is the smallest of all attached
    /// clients and so may be smaller than the one requested.
    pub size: Size,
    /// Whether this host takes pasted files. False on a host running a build
    /// from before pasting existed, which is worth saying out loud rather than
    /// leaving a key that does nothing.
    pub paste: bool,
    /// Whether this host can be scrolled back through, for the same reason and
    /// with the same answer when it cannot.
    pub scroll: bool,
    /// Whether this host takes a title from an attached client, again for the
    /// same reason: a rename key that quietly did nothing would read as a
    /// broken client rather than an old host.
    pub rename: bool,
    /// Whether this host puts its other sessions' events on this stream, which
    /// is what rings the terminal for a bell in the session next door. Nothing
    /// here reads it: a host that cannot ring is quiet, and a sentence about it
    /// on every attach is noisier than the bells it is about. Kept because a
    /// client from the build that did say so is still out there reading it, and
    /// a node that stopped answering would have it call every new host old.
    pub events: bool,
    /// Whether this client only watches. The node enforces it, so this is what
    /// the client draws and which keys it bothers offering, not what makes it
    /// safe.
    pub read_only: bool,
}

/// The two halves of an attached session, so output can be read while input is
/// being sent.
pub struct SessionHalves {
    pub reader: SessionReader,
    pub writer: SessionWriter,
}

impl Attached {
    pub fn split(self) -> SessionHalves {
        // The ssh process outlives both halves: whichever is dropped last takes
        // it with them.
        let carrier = self.carrier.map(Arc::new);
        SessionHalves {
            reader: SessionReader {
                read: self.read,
                _carrier: carrier.clone(),
                deadline: None,
            },
            writer: SessionWriter {
                write: self.write,
                _carrier: carrier,
            },
        }
    }
}

/// Something that happened in the session.
#[derive(Debug)]
pub enum Update {
    /// Terminal output. The first one after attaching repaints the screen.
    Output(Vec<u8>),
    /// The screen as the host has it, in answer to [`SessionWriter::resync`].
    ///
    /// Written to the terminal like any other output, but never treated as the
    /// session doing something: a dump paints both screen buffers, so taking
    /// its screen switches for the session's would ask for another dump
    /// forever.
    Screen(Vec<u8>),
    /// Lines of the session's history, sent before the repaint when a client
    /// asked for them. Written to the terminal as they are, so its own
    /// scrollback holds what the session printed before this attach.
    History(Vec<u8>),
    /// A window of the session's history, in answer to
    /// [`SessionWriter::view`]. What a client scrolling back paints.
    View(proto::View),
    /// Where a search found what it was looking for, in answer to
    /// [`SessionWriter::find`].
    Found(proto::Found),
    /// What the session is called now, in answer to [`SessionWriter::rename`],
    /// or why it is still called what it was.
    Renamed(proto::Renamed),
    /// The size the session settled on, which is not always the one that was
    /// asked for: the node takes the smallest across every attached client.
    ///
    /// Nothing to do for a client that draws on the terminal in front of it,
    /// which is told its own size by the terminal and lets the node clip. It
    /// is for a client keeping a screen of its own, which has to reflow that
    /// copy to the shape the session actually took.
    Resized(Size),
    /// The host is checking this client is still there. Answer it with
    /// [`SessionWriter::pong`], or the host will eventually conclude the client
    /// went away and detach it. Answering once is what opts a client in: one
    /// that never does is held to no deadline.
    Ping,
    /// Something happened in another session on the same machine: a bell, a
    /// notification, a session that exited badly. Sent unasked while attached,
    /// so that the person sitting in one session hears the one next door.
    Event(HostedEvent),
    /// The session's process exited.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

pub struct SessionReader {
    read: Reader,
    _carrier: Option<Arc<Carrier>>,
    /// When to stop believing the host is there, or `None` until it has pinged.
    deadline: Option<Instant>,
}

impl SessionReader {
    /// The next thing the host had to say, or that it has stopped saying
    /// anything.
    ///
    /// The deadline is the client's half of the ping contract, and it is what
    /// makes a dropped connection something the client finds out about. A
    /// connection that closes is the easy case and arrives here as the end of
    /// the stream; one that dies without closing sends nothing and stays open,
    /// and there is no layer below this that will ever say so.
    ///
    /// It is a moment rather than a stretch of patience per call, because this
    /// is polled from a `select!` that wakes on every keystroke: a deadline
    /// starting again with each call would be pushed back for as long as
    /// somebody keeps typing, which is exactly what a person does to a session
    /// that has stopped answering. Cancelling the read is otherwise free, since
    /// the frames are decoded through a reader that keeps what it had.
    pub async fn next(&mut self) -> Result<Update> {
        loop {
            let frame = match self.deadline {
                Some(at) => match tokio::time::timeout_at(at, self.read.next()).await {
                    Ok(frame) => frame?,
                    Err(_) => return Ok(Update::Disconnected),
                },
                None => self.read.next().await?,
            };
            let Some(frame) = frame else {
                return Ok(Update::Disconnected);
            };
            // Anything at all counts as the host being there, so a session
            // printing steadily is never given up on between two pings.
            if self.deadline.is_some() {
                self.deadline = Some(Instant::now() + proto::SILENT_FOR);
            }
            return Ok(match frame.tag {
                tag::DATA => Update::Output(frame.body),
                tag::RESYNC => Update::Screen(frame.body),
                tag::HISTORY => Update::History(frame.body),
                tag::VIEW => Update::View(proto::decode(&frame.body)?),
                tag::FIND => Update::Found(proto::decode(&frame.body)?),
                tag::RENAME => Update::Renamed(proto::decode(&frame.body)?),
                tag::SIZE => Update::Resized(proto::decode(&frame.body)?),
                // The first ping is what opts this connection into the deadline
                // above, the mirror of the host holding only a client that has
                // answered one. A node too old to ping says nothing for hours
                // at a time quite legitimately, and giving up on it would make
                // every idle session on an older machine look like a dead
                // connection.
                tag::PING => {
                    self.deadline = Some(Instant::now() + proto::SILENT_FOR);
                    Update::Ping
                }
                tag::EVENT => Update::Event(proto::decode(&frame.body)?),
                tag::EXIT => Update::Exited(proto::decode(&frame.body)?),
                // Unknown tags are skipped rather than fatal, so a newer host
                // can add frames without breaking older clients.
                _ => continue,
            });
        }
    }
}

pub struct SessionWriter {
    write: Writer,
    _carrier: Option<Arc<Carrier>>,
}

impl SessionWriter {
    pub async fn send_input(&mut self, bytes: &[u8]) -> Result<()> {
        proto::write_frame(&mut self.write, tag::DATA, bytes).await
    }

    /// Send a file from this machine's clipboard to the session's host, which
    /// writes it down and pastes the path into the session.
    ///
    /// Sent in chunks because a screenshot is bigger than a frame, and finished
    /// with the tag that says the chunks add up to a whole file: a transfer cut
    /// off part way through is one the host never acts on.
    pub async fn send_paste(&mut self, kind: &str, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(proto::PASTE_CHUNK) {
            proto::write_frame(&mut self.write, tag::PASTE, chunk).await?;
        }
        let info = PasteInfo {
            kind: kind.to_string(),
        };
        proto::write_msg(&mut self.write, tag::PASTE_END, &info).await
    }

    pub async fn resize(&mut self, size: Size) -> Result<()> {
        proto::write_msg(&mut self.write, tag::RESIZE, &size).await
    }

    pub async fn detach(&mut self) -> Result<()> {
        proto::write_frame(&mut self.write, tag::DETACH, &[]).await
    }

    /// Ask for the screen again, answered with an [`Update::Screen`].
    ///
    /// For when the client has swallowed something the terminal would have
    /// redrawn from: a session switching between the primary and alternate
    /// screens never reaches the terminal, so the picture on the other side of
    /// the switch has to come from the node's model instead.
    pub async fn resync(&mut self) -> Result<()> {
        proto::write_frame(&mut self.write, tag::RESYNC, &[]).await
    }

    /// Ask for a window of the session's history, answered with an
    /// [`Update::View`].
    ///
    /// A block rather than a screenful: scrolling inside what has already
    /// arrived costs nothing, and a request per wheel notch would be a round
    /// trip over ssh per wheel notch.
    pub async fn view(&mut self, request: &ViewRequest) -> Result<()> {
        proto::write_msg(&mut self.write, tag::VIEW, request).await
    }

    /// Search everything the session has printed, answered with an
    /// [`Update::Found`] naming every line that matches.
    pub async fn find(&mut self, needle: &str) -> Result<()> {
        let request = FindRequest {
            needle: needle.to_string(),
        };
        proto::write_msg(&mut self.write, tag::FIND, &request).await
    }

    /// Rename the session, the same thing `mm rename` asks for from outside,
    /// answered with an [`Update::Renamed`].
    ///
    /// Answered because the host can refuse: the name may be another session's,
    /// or nothing may survive the sanitising every session name goes through.
    /// Until the answer arrives the client is still showing the old name, which
    /// is the right thing to be showing.
    pub async fn rename(&mut self, name: &str) -> Result<()> {
        proto::write_msg(&mut self.write, tag::RENAME, &name).await
    }

    /// Answer an [`Update::Ping`], which is how the host tells a client that is
    /// still there from one whose connection died without closing.
    pub async fn pong(&mut self) -> Result<()> {
        proto::write_frame(&mut self.write, tag::PONG, &[]).await
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Longer than any deadline here, so a reader that is waiting for the right
    /// reason still finishes the test rather than hanging it.
    const LONGER_THAN_ANY_WAIT: Duration = Duration::from_secs(600);

    fn reading(stream: impl AsyncRead + Unpin + Send + 'static) -> SessionReader {
        let read: Box<dyn AsyncRead + Unpin + Send> = Box::new(stream);
        SessionReader {
            read: FrameReader::new(read),
            _carrier: None,
            deadline: None,
        }
    }

    /// The reason the client cannot simply wait for the stream to end. A lid
    /// closed on a wifi hop kills the connection without closing it: ssh does
    /// not notice, so the pipe stays open and silent, and a reader with no
    /// deadline of its own sits there believing it is still attached while the
    /// node has long since given up its half.
    #[tokio::test(start_paused = true)]
    async fn a_host_that_stops_answering_is_a_connection_that_went() {
        let (mut host, client) = tokio::io::duplex(4096);
        let mut reader = reading(client);

        proto::write_frame(&mut host, tag::PING, &[]).await.unwrap();
        assert!(matches!(reader.next().await.unwrap(), Update::Ping));

        // Silence, with the stream still open: `host` is deliberately never
        // dropped, because a connection that closed is the easy case.
        let update = tokio::time::timeout(LONGER_THAN_ANY_WAIT, reader.next())
            .await
            .expect("a host that went quiet should be given up on, not waited on forever")
            .unwrap();
        assert!(matches!(update, Update::Disconnected), "got {update:?}");
    }

    /// The other half of the ping contract, and the reason the deadline is not
    /// simply always on. A node too old to ping says nothing for hours at a
    /// time quite legitimately, and dropping it would make every idle session
    /// on an older machine look like a dead connection.
    #[tokio::test(start_paused = true)]
    async fn a_host_too_old_to_ask_is_never_given_up_on() {
        let (mut host, client) = tokio::io::duplex(4096);
        let mut reader = reading(client);

        proto::write_frame(&mut host, tag::DATA, b"hello")
            .await
            .unwrap();
        assert!(matches!(reader.next().await.unwrap(), Update::Output(_)));

        let waited = tokio::time::timeout(LONGER_THAN_ANY_WAIT, reader.next()).await;
        assert!(
            waited.is_err(),
            "a host that has never pinged is an older build, not a connection that went"
        );
    }

    /// The deadline is a moment, not a stretch of patience per call. `next` is
    /// polled from a `select!` that wakes on every keystroke, so a deadline
    /// starting again with each call would be pushed back for as long as
    /// somebody keeps typing, which is exactly what a person does to a session
    /// that has stopped answering.
    #[tokio::test(start_paused = true)]
    async fn typing_at_a_host_that_went_does_not_put_off_giving_up() {
        let (mut host, client) = tokio::io::duplex(4096);
        let mut reader = reading(client);

        proto::write_frame(&mut host, tag::PING, &[]).await.unwrap();
        assert!(matches!(reader.next().await.unwrap(), Update::Ping));

        // A keystroke every few seconds, each one cancelling the read the way
        // the other arms of the pump's `select!` do.
        for _ in 0..30 {
            let cancelled = tokio::time::timeout(Duration::from_secs(3), reader.next()).await;
            if let Ok(update) = cancelled {
                assert!(
                    matches!(update.unwrap(), Update::Disconnected),
                    "the only thing this stream can produce is the giving up"
                );
                return;
            }
        }
        panic!("a silent host outlasted 90 seconds of keystrokes");
    }
}
