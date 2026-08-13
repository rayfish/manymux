//! The client half.
//!
//! Everything here is transport-agnostic and terminal-agnostic: a [`Stream`] is
//! a Unix-socket connection to the node on this machine, or the pipes of an
//! `ssh <host> mm agent` running on another one. An [`Attached`] session
//! hands out bytes rather than driving a terminal. The CLI is one consumer; a
//! mobile app rendering the session with its own terminal widget is another.

pub mod attach;
pub mod status;

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::process::Child;

use crate::proto::{self, Request, Response, Size, tag};

type Reader = Box<dyn AsyncRead + Unpin + Send>;
type Writer = Box<dyn AsyncWrite + Unpin + Send>;

/// The ssh process carrying a stream to another machine. Dropping it hangs up,
/// which is why it travels with the stream rather than being left behind.
struct Carrier {
    _child: Child,
}

/// One request/response exchange, which an `Attach` extends into a byte pipe.
pub struct Stream {
    read: Reader,
    write: Writer,
    carrier: Option<Carrier>,
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
            read: Box::new(read),
            write: Box::new(write),
            carrier: None,
        })
    }

    /// Reach another machine by running `mm agent` there over ssh.
    ///
    /// `host` is an ssh destination, so anything your ssh config can reach,
    /// this can reach, and sshd decides whether you are allowed in.
    pub async fn over_ssh(host: &str) -> Result<Self> {
        let agent = crate::ssh::agent(host)?;
        Ok(Self {
            read: Box::new(agent.stdout),
            write: Box::new(agent.stdin),
            carrier: Some(Carrier {
                _child: agent.child,
            }),
        })
    }

    /// Wrap an already-open pair of stream halves. The escape hatch for tests
    /// and for transports this module doesn't know about.
    pub fn from_halves(read: Reader, write: Writer) -> Self {
        Self {
            read,
            write,
            carrier: None,
        }
    }

    /// Send a request and read its response.
    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        proto::write_msg(&mut self.write, tag::REQUEST, request).await?;
        let Some(frame) = proto::read_frame(&mut self.read).await? else {
            // An ssh that failed to connect, or a remote with no `mm` on it,
            // leaves nothing on the pipe. Its exit status says far more than
            // "the stream ended", so go and look.
            if let Some(carrier) = &mut self.carrier
                && let Ok(Some(status)) = carrier._child.try_wait()
            {
                bail!("ssh exited with {status} before answering");
            }
            bail!("the host closed the connection without responding");
        };
        if frame.tag != tag::RESPONSE {
            bail!("expected a response, got tag {:#x}", frame.tag);
        }
        proto::decode(&frame.body)
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
            let Some(frame) = proto::read_frame(&mut self.read).await? else {
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
    pub async fn attach(mut self, name: &str, size: Size) -> Result<Attached> {
        let request = Request::Attach {
            name: name.to_string(),
            size,
        };
        match self.request(&request).await? {
            Response::Attached { size } => Ok(Attached {
                read: self.read,
                write: self.write,
                carrier: self.carrier,
                size,
            }),
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
    /// The session's process exited.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

pub struct SessionReader {
    read: Reader,
    _carrier: Option<Arc<Carrier>>,
}

impl SessionReader {
    pub async fn next(&mut self) -> Result<Update> {
        loop {
            let Some(frame) = proto::read_frame(&mut self.read).await? else {
                return Ok(Update::Disconnected);
            };
            return Ok(match frame.tag {
                tag::DATA => Update::Output(frame.body),
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

    pub async fn resize(&mut self, size: Size) -> Result<()> {
        proto::write_msg(&mut self.write, tag::RESIZE, &size).await
    }

    pub async fn detach(&mut self) -> Result<()> {
        proto::write_frame(&mut self.write, tag::DETACH, &[]).await
    }
}
