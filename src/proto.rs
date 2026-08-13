//! Wire protocol shared by the server, the CLI, and the laptop daemon.
//!
//! One exchange happens per stream: a client opens a stream, sends a
//! [`Request`], and reads a [`Response`]. An `Attach` request turns the rest of
//! the stream into a bidirectional pipe carrying the session's bytes.
//!
//! A "stream" is a Unix-socket connection locally and a QUIC bi-stream under
//! iroh, so everything here is generic over `AsyncRead`/`AsyncWrite` and the
//! transport swap costs nothing.
//!
//! Frames are `[tag: u8][len: u32 BE][body]`. Control bodies are msgpack; data
//! bodies are raw terminal bytes, which is why this isn't one msgpack enum:
//! keeping PTY output out of the serializer means the hot path is a copy.

use anyhow::{Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// ALPN for the iroh transport. This is the only compatibility gate: bump it in
/// the same change as any incompatible protocol change.
pub const ALPN: &[u8] = b"manymux/v1";

/// Frames larger than this are a protocol error, not an allocation.
pub const MAX_FRAME: usize = 1 << 20;

pub mod tag {
    pub const REQUEST: u8 = 0x01;
    pub const RESPONSE: u8 = 0x02;
    /// Raw terminal bytes. Client to server it is keystrokes, server to client
    /// it is PTY output.
    pub const DATA: u8 = 0x10;
    pub const RESIZE: u8 = 0x11;
    pub const EXIT: u8 = 0x12;
    pub const DETACH: u8 = 0x13;
    /// A session event on a subscription stream: a bell, a title change, an
    /// exit. Server to client only, and unsolicited.
    pub const EVENT: u8 = 0x14;
    /// Liveness probe on an attached stream, and its answer.
    ///
    /// The only way the host learns that a client vanished without detaching.
    /// A client that answers one of these is expected to keep answering; one
    /// that never answers is an older client and is left alone.
    pub const PING: u8 = 0x15;
    pub const PONG: u8 = 0x16;
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Size {
    pub cols: u16,
    pub rows: u16,
}

impl Size {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self { cols, rows }
    }

    /// Clamp to something a terminal can actually be.
    ///
    /// A zero dimension means the client could not query its tty (running under
    /// `script`, or a pipe). Treat it as unknown and use the default rather
    /// than clamping to 1: the effective size is the smallest attached client,
    /// so a 1x1 client would shrink a live session to a single cell and throw
    /// the screen away.
    pub fn sane(self) -> Self {
        let default = Self::default();
        Self {
            cols: if self.cols == 0 {
                default.cols
            } else {
                self.cols.min(1000)
            },
            rows: if self.rows == 0 {
                default.rows
            } else {
                self.rows.min(1000)
            },
        }
    }
}

impl Default for Size {
    fn default() -> Self {
        Self { cols: 80, rows: 24 }
    }
}

// A PTY is a host-side concept, so this conversion does not exist in the
// client-only build a mobile app links against.
#[cfg(feature = "desktop")]
impl From<Size> for pty_process::Size {
    fn from(s: Size) -> Self {
        pty_process::Size::new(s.rows, s.cols)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SpawnSpec {
    /// Session name. Defaults to the command's basename, deduplicated.
    pub name: Option<String>,
    /// Empty means the user's login shell.
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub size: Size,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    List,
    Spawn(SpawnSpec),
    Kill {
        name: String,
    },
    Rename {
        name: String,
        title: String,
    },
    Attach {
        name: String,
        size: Size,
    },
    /// Turn this stream into a feed of everything happening in this machine's
    /// sessions. What lets a bell reach you when nothing is attached to see it.
    Events,

    /// Stop the node. Every session it owns dies with it, so this is for
    /// picking up a new binary, and the caller is expected to have asked.
    Stop,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Response {
    Sessions(Vec<SessionInfo>),
    Spawned {
        name: String,
    },
    Ok,
    /// The stream is now a byte pipe. Everything after this is `DATA`,
    /// `RESIZE`, `DETACH` or `EXIT`.
    Attached {
        size: Size,
    },
    Error(String),
}

/// Something that happened in a session, as it goes over the wire.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionEvent {
    pub session: String,
    /// What the session calls itself, so a notification can say something
    /// better than the session's short name.
    pub title: String,
    pub kind: EventKind,
    /// How many clients were attached when this happened. Zero is the case
    /// worth interrupting someone over: nobody saw it.
    pub attached: usize,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum EventKind {
    /// BEL: the classic "look at me".
    Bell,
    /// An explicit desktop notification request (OSC 9 or OSC 777).
    Notify { title: String, body: String },
    /// The program changed its terminal title.
    TitleChanged(String),
    /// The session's process exited.
    Exited(i32),
    /// A session was started on this host.
    Started,
}

/// A session event, tagged with the host it happened on. What the daemon's
/// aggregated feed carries, where a single host's feed carries bare
/// [`SessionEvent`]s.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HostedEvent {
    pub host: String,
    pub event: SessionEvent,
}

/// A session together with the host it is running on.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct HostedSession {
    pub host: String,
    pub session: SessionInfo,
}

/// A host the daemon could not reach, and why.
///
/// Reported alongside the sessions rather than instead of them: one machine
/// being asleep must not hide the others.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct UnreachableHost {
    pub host: String,
    pub error: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SessionInfo {
    pub name: String,
    /// What the session calls itself: a sticky title from `mm rename`, else
    /// the last OSC title the program set, else the command.
    pub title: String,
    pub command: String,
    pub pid: u32,
    pub size: Size,
    pub attached: usize,
    /// Seconds since the session last produced output.
    pub idle: u64,
    /// Bells rung since the last attach.
    pub bells: u64,
}

pub async fn write_frame<W>(w: &mut W, tag: u8, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    if body.len() > MAX_FRAME {
        bail!(
            "frame of {} bytes exceeds the {MAX_FRAME} limit",
            body.len()
        );
    }
    let mut head = [0u8; 5];
    head[0] = tag;
    head[1..].copy_from_slice(&(body.len() as u32).to_be_bytes());
    w.write_all(&head).await?;
    w.write_all(body).await?;
    w.flush().await?;
    Ok(())
}

pub async fn write_msg<W, T>(w: &mut W, tag: u8, msg: &T) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    write_frame(w, tag, &encode(msg)?).await
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    Ok(rmp_serde::to_vec_named(msg)?)
}

/// One protocol frame: a tag saying what it is, and its payload.
#[derive(Debug, Clone)]
pub struct Frame {
    pub tag: u8,
    pub body: Vec<u8>,
}

/// Read one frame. `Ok(None)` is a clean end of stream.
pub async fn read_frame<R>(r: &mut R) -> Result<Option<Frame>>
where
    R: AsyncRead + Unpin,
{
    let mut head = [0u8; 5];
    match r.read_exact(&mut head).await {
        Ok(_) => {}
        Err(e) if is_eof(&e) => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        bail!("peer sent a {len} byte frame, over the {MAX_FRAME} limit");
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body).await?;
    Ok(Some(Frame { tag: head[0], body }))
}

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(rmp_serde::from_slice(body)?)
}

fn is_eof(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unknown_size_falls_back_rather_than_collapsing_to_one_cell() {
        assert_eq!(Size::new(0, 0).sane(), Size::default());
        assert_eq!(Size::new(0, 40).sane(), Size::new(80, 40));
        assert_eq!(Size::new(120, 0).sane(), Size::new(120, 24));
    }

    #[test]
    fn real_sizes_are_left_alone_but_capped() {
        assert_eq!(Size::new(120, 40).sane(), Size::new(120, 40));
        assert_eq!(Size::new(9000, 9000).sane(), Size::new(1000, 1000));
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(4096);
        write_msg(&mut a, tag::REQUEST, &Request::List)
            .await
            .unwrap();
        write_frame(&mut a, tag::DATA, b"raw bytes").await.unwrap();

        let frame = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(frame.tag, tag::REQUEST);
        assert!(matches!(
            decode::<Request>(&frame.body).unwrap(),
            Request::List
        ));

        let frame = read_frame(&mut b).await.unwrap().unwrap();
        assert_eq!(frame.tag, tag::DATA);
        assert_eq!(frame.body, b"raw bytes");

        drop(a);
        assert!(read_frame(&mut b).await.unwrap().is_none());
    }
}
