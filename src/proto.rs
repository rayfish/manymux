//! Wire protocol shared by the server, the CLI, and the laptop daemon.
//!
//! One exchange happens per stream: a client opens a stream, sends a
//! [`Request`], and reads a [`Response`]. An `Attach` request turns the rest of
//! the stream into a bidirectional pipe carrying the session's bytes.
//!
//! A "stream" is a Unix-socket connection to the node on this machine, or the
//! pipes of an `ssh <host> mm agent` for one on another, so everything here is
//! generic over `AsyncRead`/`AsyncWrite` and a new transport costs nothing.
//!
//! Frames are `[tag: u8][len: u32 BE][body]`. Control bodies are msgpack; data
//! bodies are raw terminal bytes, which is why this isn't one msgpack enum:
//! keeping PTY output out of the serializer means the hot path is a copy.
//!
//! Nothing here negotiates a version, because the two machines on a stream are
//! routinely running different builds: a fleet gets updated one host at a time.
//! What keeps that working is that both ends skip a tag they do not know, which
//! is what makes a new frame kind safe to add, and that an undecodable
//! [`Request`] is answered with a complaint naming the version rather than a
//! closed connection. Changing the framing itself has neither, and would need a
//! way to tell the two apart before it could be done at all.

use anyhow::{Result, bail};
use bytes::BytesMut;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, FramedRead};

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
    /// A chunk of a file pasted from the clipboard of the machine the client is
    /// sitting at. Client to server only, and meaningless on its own: the
    /// chunks are appended until [`PASTE_END`] says the file is whole.
    pub const PASTE: u8 = 0x17;
    /// The end of a paste, carrying the [`super::PasteInfo`] describing what
    /// the chunks before it add up to.
    pub const PASTE_END: u8 = 0x18;
    /// The screen again: empty from the client asking, and the screen itself
    /// coming back.
    ///
    /// The client asks when it has swallowed something from the session that
    /// the terminal would otherwise have redrawn for it, which means a switch
    /// between the primary and alternate screens. The answer is tagged rather
    /// than sent as [`DATA`] because a dump paints both screen buffers and so
    /// carries switches of its own: seeing those as the session's would have
    /// the client asking for another screen forever.
    ///
    /// A node too old to know the tag skips it, and the screen stays as it was
    /// until the session next paints, which is what happened before it existed.
    pub const RESYNC: u8 = 0x19;

    /// Lines of a session's history, sent before the repaint to a client that
    /// asked for them, so the terminal it is sitting at has something in its
    /// own scrollback. Server to client, chunked when it is large.
    ///
    /// Its own tag rather than part of the repaint because the client has to
    /// scroll what it writes out of the way before the screen is painted over
    /// it, and where the screen starts is not something the node can know: the
    /// mark and its scrolling region are the client's.
    ///
    /// A node too old to know the tag sends none, and a new node sends them
    /// only to a client that asked, so the tag never reaches one that would
    /// have to skip it.
    pub const HISTORY: u8 = 0x1a;
}

/// The largest file a paste may carry. A screenshot is a couple of megabytes;
/// past this someone is sending a video, and the host would be holding all of
/// it in memory while they did.
pub const MAX_PASTE: usize = 16 << 20;

/// How much of a pasted file goes in one frame. Well under [`MAX_FRAME`], and
/// large enough that a few megabytes is tens of frames rather than thousands.
pub const PASTE_CHUNK: usize = 256 << 10;

/// What the host needs to know about the bytes a paste just sent it.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PasteInfo {
    /// File extension, sniffed from the bytes by the client: `png`, `jpg`,
    /// `gif`, `webp`. The host sanitises it before it becomes a filename.
    pub kind: String,
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
        /// Lines of history to send before the screen. Zero from a client
        /// painting on a screen of its own, where there is nowhere to put them.
        ///
        /// Defaulted rather than required, so an older client's request still
        /// decodes and attaches with no history at all.
        #[serde(default)]
        history: u32,
    },
    /// Turn this stream into a feed of everything happening in this machine's
    /// sessions. What lets a bell reach you when nothing is attached to see it.
    Events,

    /// Stop the node. Every session it owns dies with it, so this is for
    /// picking up a new binary, and the caller is expected to have asked.
    Stop,

    /// What build the node is running, so an update can tell whether
    /// restarting it would change anything.
    ///
    /// A node too old to decode this answers with an error, and that is itself
    /// the answer: nothing older than the build that introduced the request
    /// knows it, so an error means the node is behind.
    Version,
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
        /// Whether this host understands `PASTE` frames. A host too old to
        /// know the tag would skip them in silence, and a client that could
        /// not tell would leave you pressing a key that does nothing; with
        /// this it can say why instead.
        ///
        /// Defaulted rather than required, so a newer client reading an older
        /// host's answer gets `false` rather than a decode error.
        #[serde(default)]
        paste: bool,
    },
    /// What the node is running. `build` is the SHA-256 of the binary it
    /// started from, taken at startup: the version alone cannot answer the
    /// question, since the nightly channel keeps one version across builds,
    /// and the path is no better once an update has replaced the file
    /// underneath the process. `None` if the node could not hash itself.
    Version {
        version: String,
        build: Option<String>,
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
    /// How many clients are attached to any session on the machine this came
    /// from, this one included. What decides *where* a notification goes: with
    /// somebody attached there, their terminal is told (`notify::escape`) and
    /// the desktop notifier stays quiet, so one bell interrupts once.
    ///
    /// Defaulted rather than required, so an older node's events still notify
    /// the way they always did instead of failing to decode.
    #[serde(default)]
    pub host_attached: usize,
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

pub fn decode<T: DeserializeOwned>(body: &[u8]) -> Result<T> {
    Ok(rmp_serde::from_slice(body)?)
}

/// Bytes of header before a frame's body: the tag and the length.
const HEAD: usize = 5;

/// Picks frames out of a stream of bytes.
///
/// A codec rather than a pair of `read_exact` calls because both places that
/// read frames do it inside a `select!`, which drops the losing branch's future
/// wherever it happened to be. Bytes held by such a future are gone, and half a
/// header consumed and dropped desynchronises the stream for good: the next
/// read takes body content for a header. A codec keeps the partial frame in a
/// buffer belonging to the reader, so a dropped read costs nothing.
struct FrameCodec;

impl Decoder for FrameCodec {
    type Item = Frame;
    type Error = anyhow::Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Frame>> {
        if src.len() < HEAD {
            return Ok(None);
        }
        let len = u32::from_be_bytes(src[1..HEAD].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            bail!("peer sent a {len} byte frame, over the {MAX_FRAME} limit");
        }
        if src.len() < HEAD + len {
            // Ask for the whole of what is missing at once, rather than
            // growing the buffer a read at a time.
            src.reserve(HEAD + len - src.len());
            return Ok(None);
        }
        let tag = src[0];
        let body = src.split_to(HEAD + len).split_off(HEAD).to_vec();
        Ok(Some(Frame { tag, body }))
    }
}

/// The frames arriving on a stream.
pub struct FrameReader<R> {
    inner: FramedRead<R, FrameCodec>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(read: R) -> Self {
        Self {
            inner: FramedRead::new(read, FrameCodec),
        }
    }

    /// The next frame, or `None` at the end of the stream.
    ///
    /// Cancel safe: dropping this future leaves anything it had read in the
    /// buffer, which is what makes it sound to await in a `select!`.
    pub async fn next(&mut self) -> Result<Option<Frame>> {
        match self.inner.next().await {
            None => Ok(None),
            Some(Ok(frame)) => Ok(Some(frame)),
            // A peer that reset the connection has gone away, which callers
            // want to hear about the same way as a clean close. A stream that
            // ends mid-frame is a different thing and stays an error.
            Some(Err(e)) if reset(&e) => Ok(None),
            Some(Err(e)) => Err(e),
        }
    }
}

fn reset(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>()
        .is_some_and(|e| e.kind() == std::io::ErrorKind::ConnectionReset)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    /// The one place a field was added to an existing message, and the reason
    /// it could be: both directions of a fleet halfway through an update have
    /// to keep working, and neither end negotiates anything.
    #[test]
    fn an_older_hosts_attach_answer_still_decodes() {
        /// `Response::Attached` as it was before pasting existed.
        #[derive(Serialize, Deserialize)]
        enum Old {
            #[allow(dead_code)]
            Sessions(Vec<SessionInfo>),
            #[allow(dead_code)]
            Spawned {
                name: String,
            },
            #[allow(dead_code)]
            Ok,
            Attached {
                size: Size,
            },
            #[allow(dead_code)]
            Error(String),
        }

        let old = encode(&Old::Attached {
            size: Size::new(80, 24),
        })
        .unwrap();
        let decoded: Response = decode(&old).unwrap();
        let Response::Attached { size, paste } = decoded else {
            panic!("an old answer should still be an attach");
        };
        assert_eq!(size, Size::new(80, 24));
        assert!(!paste, "a host that never heard of pasting cannot take one");

        // And the other way: an old client reading a new host's answer, which
        // is a fleet updated from the far end first.
        let new = encode(&Response::Attached {
            size: Size::new(80, 24),
            paste: true,
        })
        .unwrap();
        assert!(matches!(decode::<Old>(&new).unwrap(), Old::Attached { .. }));
    }

    /// The same rule for the event feed: a node that predates the host-wide
    /// count still notifies the way it always did, rather than an event that
    /// will not decode.
    #[test]
    fn an_older_hosts_event_still_decodes() {
        /// `SessionEvent` as it was before an attached terminal could notify.
        #[derive(Serialize, Deserialize)]
        struct Old {
            session: String,
            title: String,
            kind: EventKind,
            attached: usize,
        }

        let old = encode(&Old {
            session: "api".into(),
            title: "fixing the parser".into(),
            kind: EventKind::Bell,
            attached: 0,
        })
        .unwrap();
        let event: SessionEvent = decode(&old).unwrap();
        assert_eq!(event.session, "api");
        assert_eq!(
            event.host_attached, 0,
            "a host that cannot count them has nobody attached as far as we know"
        );

        // And an old node reading a new one's event, which is a machine
        // watching a peer that was updated first.
        let new = encode(&SessionEvent {
            session: "api".into(),
            title: "fixing the parser".into(),
            kind: EventKind::Bell,
            attached: 0,
            host_attached: 2,
        })
        .unwrap();
        assert_eq!(decode::<Old>(&new).unwrap().session, "api");
    }

    /// What makes an unanswered version request an answer in itself: a node
    /// from before the request existed cannot decode it, so `mm update` reads
    /// the complaint as "older than me" rather than having to guess.
    #[test]
    fn a_node_from_before_version_cannot_decode_the_request() {
        /// `Request` as it was before the version request existed.
        #[derive(Serialize, Deserialize)]
        enum Old {
            List,
            #[allow(dead_code)]
            Spawn(SpawnSpec),
            #[allow(dead_code)]
            Kill {
                name: String,
            },
            #[allow(dead_code)]
            Rename {
                name: String,
                title: String,
            },
            #[allow(dead_code)]
            Attach {
                name: String,
                size: Size,
            },
            #[allow(dead_code)]
            Events,
            #[allow(dead_code)]
            Stop,
        }

        let asked = encode(&Request::Version).unwrap();
        assert!(
            decode::<Old>(&asked).is_err(),
            "an older node has no variant to decode this into"
        );

        // The requests it does know still arrive as themselves, so adding the
        // variant costs nothing on the machines that are behind.
        assert!(matches!(
            decode::<Old>(&encode(&Request::List).unwrap()).unwrap(),
            Old::List
        ));
    }

    /// A client that asks a node from before the field existed still attaches:
    /// the key is dropped on the way in, and the answer is the screen alone.
    #[test]
    fn an_older_node_ignores_the_history_a_client_asks_for() {
        /// `Request::Attach` as it was before history existed.
        #[derive(Serialize, Deserialize)]
        enum Old {
            Attach { name: String, size: Size },
        }

        let asked = encode(&Request::Attach {
            name: "api".into(),
            size: Size::new(80, 24),
            history: 1000,
        })
        .unwrap();
        let Old::Attach { name, size } = decode(&asked).unwrap();
        assert_eq!(name, "api");
        assert_eq!(size, Size::new(80, 24));

        // And a new node reading an older client's request, which is the same
        // fleet halfway through an update the other way round.
        let old = encode(&Old::Attach {
            name: "api".into(),
            size: Size::new(80, 24),
        })
        .unwrap();
        let Request::Attach { history, .. } = decode(&old).unwrap() else {
            panic!("an attach should decode as one");
        };
        assert_eq!(history, 0, "a client that cannot ask for history gets none");
    }

    #[test]
    fn a_version_answer_round_trips() {
        let sent = Response::Version {
            version: "0.1.0".into(),
            build: Some("abc".into()),
        };
        let Response::Version { version, build } = decode(&encode(&sent).unwrap()).unwrap() else {
            panic!("a version answer should decode as one");
        };
        assert_eq!(version, "0.1.0");
        assert_eq!(build.as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut a, b) = tokio::io::duplex(4096);
        let mut frames = FrameReader::new(b);
        write_msg(&mut a, tag::REQUEST, &Request::List)
            .await
            .unwrap();
        write_frame(&mut a, tag::DATA, b"raw bytes").await.unwrap();

        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.tag, tag::REQUEST);
        assert!(matches!(
            decode::<Request>(&frame.body).unwrap(),
            Request::List
        ));

        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.tag, tag::DATA);
        assert_eq!(frame.body, b"raw bytes");

        drop(a);
        assert!(frames.next().await.unwrap().is_none());
    }

    /// What the codec is for. Both loops that read frames do it in a `select!`,
    /// which drops the losing branch's future wherever it stood. Reading
    /// straight from the stream, the bytes that future had already taken went
    /// with it, and every frame after that was parsed one header short.
    #[tokio::test]
    async fn a_read_cancelled_part_way_through_a_frame_keeps_what_it_had() {
        let (mut a, b) = tokio::io::duplex(4096);
        let mut frames = FrameReader::new(b);

        // Three bytes of a five byte header, then a read that gives up.
        a.write_all(&[tag::DATA, 0, 0]).await.unwrap();
        let cancelled = tokio::time::timeout(Duration::from_millis(20), frames.next()).await;
        assert!(cancelled.is_err(), "half a header should not decode");

        // The rest arrives and the frame is whole, three bytes and all.
        a.write_all(&[0, 9]).await.unwrap();
        a.write_all(b"raw bytes").await.unwrap();
        let frame = frames.next().await.unwrap().unwrap();
        assert_eq!(frame.tag, tag::DATA);
        assert_eq!(frame.body, b"raw bytes");
    }
}
