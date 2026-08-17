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
use std::time::{Duration, SystemTime};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::codec::{Decoder, FramedRead};

/// Frames larger than this are a protocol error, not an allocation.
pub const MAX_FRAME: usize = 1 << 20;

/// How often the host asks an attached client whether it is still there, and
/// how long either half may go unheard before the other treats the connection
/// as gone.
///
/// Nothing underneath notices a connection that died without closing. A lid
/// closed on a wifi hop leaves sshd holding the session and ssh holding a
/// socket to nowhere; sshd's own `ClientAlive` probing is off by default, and
/// the kernel's TCP keepalive is two hours away. So the two halves say so to
/// each other, and the pair of numbers lives here rather than beside either of
/// them: a host pinging less often than the client's patience would have every
/// idle session look like a dead one, which is a way for two builds to disagree
/// that no amount of care at one end can catch.
///
/// The probe goes one way and the deadline goes both. The host learns a client
/// vanished because the answers stop, and the client learns the host went
/// because the questions do, which is the only thing that ever arrives on a
/// session that has been quietly sitting at a prompt for an hour.
pub const PING_EVERY: Duration = Duration::from_secs(15);
pub const SILENT_FOR: Duration = Duration::from_secs(45);

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

    /// A window of a session's history, asked for and answered on the same
    /// tag: the request is a [`super::ViewRequest`], the answer a
    /// [`super::View`].
    ///
    /// What a client scrolling back through a session reads, on a screen of
    /// its own where the terminal has no scrollback of its own to offer.
    pub const VIEW: u8 = 0x1b;

    /// A search through everything a session has printed, asked for and
    /// answered on the same tag: the request is a [`super::FindRequest`], the
    /// answer a [`super::Found`].
    ///
    /// Every hit comes back at once rather than one at a time, because the
    /// lines are already in memory on the node and walking the matches is then
    /// local: `n` on a machine two hops away should not be a round trip.
    pub const FIND: u8 = 0x1c;

    /// A new name for the session this stream is attached to, asked for as a
    /// msgpack string and answered on the same tag with a [`super::Renamed`].
    ///
    /// On the attached stream rather than a second connection because that is
    /// where the session already is. An [`super::Attached`] holds no socket and
    /// no host, so a client that wanted to send a `Request` would have to know
    /// how it got here, which is the one thing this half of the client is kept
    /// from knowing.
    ///
    /// Answered rather than sent and forgotten, unlike a resize: a name can be
    /// taken by another session or be nothing at all once it has been through
    /// the same sanitising a spawn goes through, and the client has the old one
    /// on its mark row until it hears which it is.
    ///
    /// A node too old to know the tag skips it in silence, which is why
    /// [`super::Response::Attached`] says whether this one does.
    pub const RENAME: u8 = 0x1d;
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
    /// What to call this in a listing, when the command is not worth showing.
    ///
    /// For the one caller whose command is a wrapper rather than the thing
    /// somebody is running: a restored session runs
    /// `sh -mc '…' sh claude --continue`, and a row that says so is a row
    /// about the machinery instead of about the work. Only the fallback title
    /// is affected, since a program that sets its own is already answering
    /// this question.
    ///
    /// Defaulted, and the one field here where being ignored costs nothing: a
    /// node too old to know it shows the wrapper, which is a worse-looking row
    /// and nothing else. Contrast `cwd`, where being ignored would start the
    /// session in the wrong place.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    List,
    Spawn(SpawnSpec),
    Kill {
        name: String,
    },
    /// Give a live session a different name, the one it is addressed by in
    /// `host/name`. The field is not the one this request used to carry, so a
    /// node too old to know it cannot decode the request and says so, which is
    /// a better answer than silently setting something else.
    Rename {
        name: String,
        to: String,
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
        /// Watch without typing: the node drops this client's input rather than
        /// trusting it not to send any, and leaves it out of the size
        /// negotiation so somebody looking on from a phone cannot shrink the
        /// screen of whoever is working.
        ///
        /// Defaulted like the rest, which is exactly why the answer carries a
        /// flag of its own: a node too old to know this field decodes the
        /// request without it and hands back a session that can be typed into.
        /// Silently. See `Response::Attached { read_only }`.
        #[serde(default)]
        read_only: bool,
    },
    /// Turn this stream into a feed of everything happening in this machine's
    /// sessions. What lets a bell reach you when nothing is attached to see it.
    Events,

    /// These machines answered a client just now, so a node that gave up
    /// watching them can start again.
    ///
    /// The node cannot work this out for itself: a client reaches another
    /// machine over its own ssh, and the node here never sees it. Being told is
    /// what replaces retrying on a timer. A node too old to decode this answers
    /// with an error, and the client throws it away, having nothing to do with
    /// the answer either way.
    Reached {
        hosts: Vec<String>,
    },

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

    /// What every session on this machine is working on, for a checkpoint.
    ///
    /// A request of its own rather than fields on [`SessionInfo`], for two
    /// reasons. Every keypress in the popup lists every machine, and reading
    /// `/proc` once per session on that path is work nobody asked for. And a
    /// node too old to know this answers with an error naming its version,
    /// which is the honest answer: a defaulted field would have come back
    /// empty and been written down as a session that was sitting at its
    /// prompt in its home directory, which is a thing the node never said.
    Snapshot,
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
        /// Whether this host answers [`tag::VIEW`], which is what a client on a
        /// screen of its own scrolls back through. False on a host from before
        /// it existed, so the key says so rather than doing nothing.
        #[serde(default)]
        scroll: bool,
        /// Whether this host takes [`tag::RENAME`], so a session can be renamed
        /// from inside it rather than only by `mm rename`. False the same way
        /// and for the same reason as the two above.
        #[serde(default)]
        rename: bool,
        /// Whether this host sends [`tag::EVENT`] on an attach stream, which is
        /// how a bell in the session next door reaches the terminal in front of
        /// you. False on a host from before it did.
        ///
        /// The one flag no client says anything about. It has no key behind it,
        /// so the only thing to say was a sentence on every attach to a host
        /// that could not ring, which is a line of chrome about something that
        /// has not happened. Still answered, because a client from the build
        /// that did say so reads it, and a node that went quiet here would have
        /// it call every new host old.
        #[serde(default)]
        events: bool,
        /// Whether this host understood the request to watch without typing.
        ///
        /// The one flag a client must not ignore. The others answer for a key
        /// that would otherwise do nothing, which is a nuisance; this one
        /// answers for a promise. A node too old to know `read_only` decodes
        /// the request without it and attaches an ordinary client, so a viewer
        /// that assumed it worked would be sitting at a live keyboard believing
        /// it could not type. `false` here means the host cannot do it, and
        /// `mm view` refuses rather than pretending.
        #[serde(default)]
        read_only: bool,
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
    /// What each session is working on. The answer to [`Request::Snapshot`].
    ///
    /// In no particular order: the caller pairs these with a listing by name
    /// and takes the order from that, which is the order every screen shows.
    /// Nothing here may be read as saying which session is which.
    Snapshot(Vec<Doing>),
    Error(String),
}

/// One session, and what it was doing when a checkpoint was taken.
///
/// Everything here describes the *foreground* of the session's terminal rather
/// than the child the node started, because the child stopped being the whole
/// story the moment somebody typed in it: what they are in the middle of is
/// whatever they ran, and where they are is wherever they last `cd`ed to.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Doing {
    pub name: String,
    /// The session leader this answer is about, which is what says the answer
    /// still describes the session the listing named. A session that ended and
    /// was replaced by another of the same name between the two questions is
    /// the one case a name cannot catch, and it would be written down as the
    /// right name doing the wrong work.
    pub pid: u32,
    /// Where the foreground process is, else where the session's leader is.
    /// `None` where the machine cannot say, which is an answer rather than a
    /// failure and is the only honest one: a session restored in the wrong
    /// directory resumes somebody else's conversation.
    #[serde(default)]
    pub cwd: Option<String>,
    /// The argv of whatever holds the terminal, which for a session sitting at
    /// a prompt is the shell itself. Empty means the machine could not say,
    /// which is *not* the same as a prompt and is not written down as one: a
    /// session at a prompt has its shell in front of it and so has an argv.
    /// Empty happens when the foreground group's leader has been reaped while
    /// the rest of a pipeline runs on, and when it is a zombie.
    ///
    /// Raw, and deliberately not a decision. Whether this is something worth
    /// starting again, and with what flag, is the client's to work out
    /// (`client::checkpoint`), for the reason a group is the client's: the
    /// list of programs that can resume themselves grows every few months, and
    /// tying it to the node would mean it grew only on the machines that had
    /// been restarted.
    #[serde(default)]
    pub foreground: Vec<String>,
}

/// A window of a session's history, as a scrolling client asks for it.
///
/// Counted back from the newest line rather than forward from the oldest,
/// because the buffer trims from the top as the session keeps printing: an
/// index from the start would name a different line a minute later, and the
/// view would drift while you read it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ViewRequest {
    /// Lines back from the newest line that the window's bottom edge sits at.
    /// Zero is the bottom of the buffer.
    pub from: u64,
    /// How many lines to send, ending at `from` and going back.
    pub lines: u32,
}

/// The answer: rendered lines, oldest first, with the pen sequences that
/// coloured them.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Where the window actually starts, which is not what was asked for when
    /// the request ran off one end of the buffer.
    pub from: u64,
    /// Lines in the whole buffer, screen included, so a client knows where the
    /// ends are without asking.
    pub total: u64,
    pub lines: Vec<String>,
}

/// What to look for in everything a session has printed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct FindRequest {
    pub needle: String,
}

/// Where it was found: one offset per matching line, counted back from the
/// newest the way [`ViewRequest::from`] is, nearest the bottom first.
///
/// The needle comes back with them so a client can tell this answer from the
/// one to a search it has since retyped.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub needle: String,
    pub lines: Vec<u64>,
}

/// What came of a rename asked for on [`tag::RENAME`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum Renamed {
    /// The name the session answers to now, which is the one asked for after
    /// the sanitising every session name goes through. The client shows this
    /// rather than what was typed, or the mark row would name a session that
    /// is not there.
    Name(String),
    /// Why the session is still called what it was: the name is another
    /// session's, or there was nothing usable left of it.
    Refused(String),
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
    /// When the session was opened. What every listing is ordered by, so that
    /// the order is the one thing about a session that never moves: a rename
    /// changes the name a listing sorts on, and a switch key that landed
    /// somewhere else afterwards is a key you cannot press without looking.
    ///
    /// Defaulted rather than required, because a node from before this existed
    /// says nothing and would otherwise fail to decode. Every session on such a
    /// host reads as the epoch, ties, and falls back to the name order it has
    /// always had.
    #[serde(default = "epoch")]
    pub started: SystemTime,
}

/// What a host too old to stamp a session with its start time is taken to have
/// said. `SystemTime` has no `Default` of its own to borrow for this.
fn epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH
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
        let Response::Attached {
            size,
            paste,
            scroll,
            rename,
            events,
            read_only,
        } = decoded
        else {
            panic!("an old answer should still be an attach");
        };
        assert!(
            !read_only,
            "nor promise to drop the input of a client that only watches"
        );
        assert_eq!(size, Size::new(80, 24));
        assert!(!paste, "a host that never heard of pasting cannot take one");
        assert!(
            !scroll,
            "nor answer for a window of a history it cannot send"
        );
        assert!(!rename, "nor rename a session on a tag it never heard of");
        assert!(
            !events,
            "nor put the session next door's bells on this stream"
        );

        // And the other way: an old client reading a new host's answer, which
        // is a fleet updated from the far end first.
        let new = encode(&Response::Attached {
            size: Size::new(80, 24),
            paste: true,
            scroll: true,
            rename: true,
            events: true,
            read_only: true,
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

    /// And for the listing, where an undecodable answer would be an `mm ls`
    /// that says a machine is unreachable when it is only running last week's
    /// build.
    #[test]
    fn an_older_hosts_listing_still_decodes() {
        /// `SessionInfo` as it was before a session was stamped with the time
        /// it opened.
        #[derive(Serialize, Deserialize)]
        struct Old {
            name: String,
            title: String,
            command: String,
            pid: u32,
            size: Size,
            attached: usize,
            idle: u64,
            bells: u64,
        }

        let old = encode(&Old {
            name: "api".into(),
            title: "zsh".into(),
            command: "zsh".into(),
            pid: 1,
            size: Size::new(80, 24),
            attached: 0,
            idle: 0,
            bells: 0,
        })
        .unwrap();
        let info: SessionInfo = decode(&old).unwrap();
        assert_eq!(info.name, "api");
        assert_eq!(
            info.started,
            SystemTime::UNIX_EPOCH,
            "a host that cannot say when a session opened leaves them all tied"
        );

        // And an old client reading a new host's listing, which is the same
        // fleet from the other end.
        let new = encode(&SessionInfo {
            name: "api".into(),
            title: "zsh".into(),
            command: "zsh".into(),
            pid: 1,
            size: Size::new(80, 24),
            attached: 0,
            idle: 0,
            bells: 0,
            started: SystemTime::now(),
        })
        .unwrap();
        assert_eq!(decode::<Old>(&new).unwrap().name, "api");
    }

    /// The other half of the rule, and why a checkpoint asks a question of its
    /// own instead of reading a field off the listing: a node too old to know
    /// the request cannot decode it, so it answers with an error naming its
    /// version. That refusal is the answer. A defaulted field would instead
    /// have come back empty and been written down as a session sitting at its
    /// prompt in its home directory, which is a thing that node never said.
    #[test]
    fn a_node_too_old_to_be_asked_what_it_is_doing_cannot_decode_the_question() {
        /// `Request` as it was before a checkpoint could be taken.
        #[derive(Serialize, Deserialize)]
        enum Old {
            List,
            Version,
        }

        let asked = encode(&Request::Snapshot).unwrap();
        assert!(
            decode::<Old>(&asked).is_err(),
            "an older node has to fail this, or it would answer something else"
        );

        // The requests it does know are untouched by the new one being added
        // after them, which is what makes adding a variant safe at all.
        assert!(matches!(
            decode::<Request>(&encode(&Old::List).unwrap()).unwrap(),
            Request::List
        ));
        assert!(matches!(
            decode::<Request>(&encode(&Old::Version).unwrap()).unwrap(),
            Request::Version
        ));
    }

    /// The one field added to an existing message here, and the reason it
    /// could be: being ignored costs a listing row that names the wrapper
    /// instead of the command, and nothing else. The rule it is held to is the
    /// contrast with `cwd`, in the same message, where being ignored would
    /// start the session somewhere nobody asked for.
    #[test]
    fn an_older_node_still_decodes_a_spawn_that_says_what_to_call_it() {
        /// `SpawnSpec` as it was before a restore had a wrapper to hide.
        #[derive(Serialize, Deserialize)]
        struct Old {
            name: Option<String>,
            command: Vec<String>,
            cwd: Option<String>,
            size: Size,
        }

        let new = encode(&SpawnSpec {
            name: Some("build".into()),
            command: vec!["sh".into(), "-mc".into()],
            cwd: Some("/tmp".into()),
            size: Size::new(80, 24),
            label: Some("claude --continue".into()),
        })
        .unwrap();
        let old: Old = decode(&new).unwrap();
        assert_eq!(old.name.as_deref(), Some("build"));
        assert_eq!(
            old.cwd.as_deref(),
            Some("/tmp"),
            "the field that matters is still read by a node that predates the one that does not"
        );

        // And the other way: this build reading a spawn from a client too old
        // to say, which is a listing row and nothing else.
        let from_old = encode(&Old {
            name: None,
            command: Vec::new(),
            cwd: None,
            size: Size::new(80, 24),
        })
        .unwrap();
        let spec: SpawnSpec = decode(&from_old).unwrap();
        assert_eq!(
            spec.label, None,
            "so the node names it the way it always has"
        );
    }

    /// A session at its prompt and a session with something running in it are
    /// told apart by an empty `foreground`, so both have to survive the trip.
    #[test]
    fn a_snapshot_round_trips_with_and_without_something_running() {
        let doing = vec![
            Doing {
                name: "build".into(),
                pid: 41823,
                cwd: Some("/srv/project".into()),
                foreground: vec!["claude".into(), "--resume".into()],
            },
            Doing {
                name: "zsh-2".into(),
                pid: 41824,
                cwd: Some("/tmp".into()),
                foreground: Vec::new(),
            },
            Doing {
                name: "elsewhere".into(),
                pid: 41825,
                cwd: None,
                foreground: Vec::new(),
            },
        ];

        let encoded = encode(&Response::Snapshot(doing.clone())).unwrap();
        let Response::Snapshot(back) = decode::<Response>(&encoded).unwrap() else {
            panic!("a snapshot should decode as one");
        };
        assert_eq!(back, doing);
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

    /// A rename used to set a title and now moves the session's name, which is
    /// a different thing done to the same session. A node that still does the
    /// old one has no business doing it on a request that meant the new one, so
    /// the field is not the field it knows: it cannot decode the request and
    /// says so, and "that machine is behind" is a better answer than a session
    /// quietly retitled instead of renamed.
    #[test]
    fn a_node_that_still_renames_titles_cannot_decode_a_rename() {
        /// `Request::Rename` as it was when it set a title.
        #[derive(Serialize, Deserialize)]
        enum Old {
            Rename { name: String, title: String },
        }

        let asked = encode(&Request::Rename {
            name: "api".into(),
            to: "build".into(),
        })
        .unwrap();
        assert!(decode::<Old>(&asked).is_err());
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
            read_only: false,
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

    /// Why a new request can go anywhere in the enum, which is the whole of
    /// "adding a `Request` variant is safe". Encoded by index instead, adding
    /// one in the middle would renumber every variant after it, and a fleet
    /// updated one machine at a time would have old clients asking for
    /// something else entirely by the same number.
    #[test]
    fn a_request_is_encoded_by_the_name_of_its_variant() {
        let encoded = encode(&Request::Version).unwrap();
        let text = String::from_utf8_lossy(&encoded);
        assert!(text.contains("Version"), "encoded as {text:?}");

        let added_later = encode(&Request::Reached {
            hosts: vec!["gpu-box".into()],
        })
        .unwrap();
        let Request::Reached { hosts } = decode(&added_later).unwrap() else {
            panic!("a reached should decode as one");
        };
        assert_eq!(hosts, ["gpu-box"]);
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
