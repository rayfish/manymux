//! A session: one PTY, one child process, and the screen state needed to make
//! reattaching look like you never left.
//!
//! The client is not the child's terminal. The PTY master stays open here for
//! the session's whole life, so a client going away is invisible to the child:
//! no SIGHUP, no EOF on stdin, nothing. That is the entire point of the
//! project, and it is why the PTY is owned by the server rather than proxied.
//! A replacement screen and its output subscription are taken under the same
//! lock as an attach, so bytes already in the screen are never replayed over it.
//! A separate image upload retains this session before receiving bytes, so a
//! rename cannot redirect the completed paste to a replacement session.
//! The reaper stops the PTY reader and drains buffered bytes before reporting
//! exit. Descendants holding a slave fd cannot retain a dead session forever.
//! The name's watch channel owns its current value and notifies every attach,
//! so a rename made by another client also changes the name used to reconnect.
//! Input queues at most sixteen protocol frames before applying backpressure.
//! Resizes keep only the latest geometry and share the same PTY writer.

use std::collections::HashMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::fd::AsFd;
use std::path::Path;
use std::slice;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result, bail};
use avt::Vt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc::Permit;
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, warn};

use super::events::{Event, Scanner};
use crate::hosts::this_machine;
use crate::lock::held;
use crate::proto::{
    self, Doing, EventKind, Found, HostedEvent, Peek, SessionEvent, SessionInfo, Size, SpawnSpec,
    View, ViewRequest,
};
use crate::shell;
use crate::user;
use crate::utf8::Utf8Decoder;

/// Lines of scrollback kept per session. Enough to scroll back through a long
/// build; small enough that a hundred sessions don't matter.
const SCROLLBACK: usize = 10_000;

/// Output chunks buffered per attached client before it is considered lagging.
/// A lagging client is resynced from the screen dump rather than disconnected.
const OUTPUT_BACKLOG: usize = 256;

/// Each queued frame is bounded by `MAX_FRAME`, including library callers.
const INPUT_BACKLOG: usize = 16;

pub struct Session {
    /// What the session is addressed by, in `host/name` and everywhere else.
    /// The watch channel keeps its readers and attached clients on the same
    /// value when the registry moves the name.
    name: watch::Sender<String>,
    pub command: String,
    pub pid: u32,
    /// When this was opened. Outside the lock above because it is the one
    /// thing about a session that nothing changes, which is exactly why every
    /// listing is ordered by it.
    pub started: SystemTime,
    input_tx: mpsc::Sender<Vec<u8>>,
    size_tx: watch::Sender<Size>,
    output_tx: broadcast::Sender<Arc<[u8]>>,
    state: Mutex<State>,
    /// Resolves to the child's exit code once it exits.
    exit_rx: watch::Receiver<Option<i32>>,
    /// The node's event bus. Bells and title changes go here, stamped with
    /// this machine's name, so a laptop can hear about them with nothing
    /// attached.
    events: broadcast::Sender<HostedEvent>,
    /// Clients attached to any session on this host, shared by all of them.
    /// Stamped onto every event, because it decides whether a bell is shown to
    /// somebody sitting in a terminal here or raised on a desktop elsewhere.
    host_clients: Arc<AtomicUsize>,
    next_client: AtomicU64,
}

struct State {
    vt: Vt,
    scanner: Scanner,
    decoder: Utf8Decoder,
    /// Lines the buffer has thrown away since the session started.
    ///
    /// `avt` keeps [`SCROLLBACK`] lines and drops the oldest to stay inside
    /// that, so how many lines it holds stops saying how many the session has
    /// printed the moment it is full. Counted here because the count of
    /// dropped lines is only offered once, as the feed that dropped them
    /// returns, and a client scrolled back needs it: see [`View::printed`].
    trimmed: u64,
    /// What `trimmed` plus the lines in the buffer came to when the count was
    /// last worked out, so that only what has been added since is added on.
    counted: u64,
    /// Where the newest line has got to, which is what a client is told and is
    /// worked out rather than measured: see [`Self::printed`].
    printed: u64,
    bells: u64,
    last_activity: Instant,
    /// Requested size per attached client. The effective size is the smallest
    /// of these, so no attached client ever sees a truncated screen.
    clients: HashMap<u64, Size>,
    size: Size,
}

impl State {
    /// A session's state before anything has been printed.
    fn new(size: Size) -> Self {
        Self {
            vt: Vt::builder()
                .size(size.cols as usize, size.rows as usize)
                .scrollback_limit(SCROLLBACK)
                .build(),
            scanner: Scanner::default(),
            decoder: Utf8Decoder::default(),
            trimmed: 0,
            counted: 0,
            printed: 0,
            bells: 0,
            last_activity: Instant::now(),
            clients: HashMap::new(),
            size,
        }
    }

    /// What the session calls itself: the last title the program set, else the
    /// command it was started with. It follows the program rather than the
    /// session's name, which is what a rename changes: the name says which
    /// session this is, and the title says what it is doing.
    fn title(&self, command: &str) -> String {
        self.scanner
            .title()
            .filter(|title| !title.is_empty())
            .unwrap_or(command)
            .to_string()
    }

    /// The screen exactly as it stands: `avt`'s dump, then everything it does
    /// not model (title, mouse reporting, bracketed paste, cursor style).
    /// Without the second half, reattaching to a full-screen program would
    /// leave its mouse dead.
    fn repaint(&self) -> String {
        format!("{}{}", self.vt.dump(), self.scanner.replay())
    }

    /// How far the newest line has got, for a client working out how far it
    /// moved since the last time it asked ([`View::printed`]).
    ///
    /// Lines thrown away plus lines still here is what the session has printed,
    /// and while it goes on printing that is the whole of it. What this is for
    /// is the three things that put lines *back*, none of which is a session
    /// unprinting anything:
    ///
    /// A full-screen program takes a screen of its own and the history is put
    /// aside behind it, so there are two dozen lines here and no scrollback at
    /// all until it quits. Counted, quitting `less` would read as nine
    /// thousand lines printed at once and throw a view somebody was holding to
    /// the top of the buffer, so nothing is counted while one is up: the
    /// buffer this counts is untouched throughout, and is the same size on the
    /// way out as it was on the way in.
    ///
    /// A reset (`\ec`) empties the buffer without trimming it, which is not
    /// anything a count of lines can describe: what a client was reading is
    /// not there to be found by any number. So the count stands where it was
    /// and goes on from what is here now, which keeps it counting the lines
    /// printed after one rather than stopping until the buffer has grown back
    /// past where it was. A reflow is the same and is dealt with where it
    /// happens, in [`Self::resize`].
    fn printed(&mut self, total: u64) -> u64 {
        if self.scanner.on_alternate() {
            return self.printed;
        }
        let here = self.trimmed + total;
        self.printed += here.saturating_sub(self.counted);
        self.counted = here;
        self.printed
    }

    /// Resize the screen model, and go on counting from whatever the reflow
    /// left rather than from what was here before it.
    ///
    /// A reflow is not the session printing or unprinting anything: a narrower
    /// screen splits the lines that were wrapped and a wider one rejoins them,
    /// so the buffer comes out of one a different length either way. Counted, a
    /// phone attaching would have said a thousand lines had just been printed
    /// and thrown every view of the session back by a thousand.
    ///
    /// Not while a full-screen program has the screen, though, and for the
    /// reason [`Self::printed`] does not count then either: `avt` reflows the
    /// buffer that is showing and leaves the other one alone, so what is being
    /// resized here is the program's own screen and the session's is put aside
    /// at the length it already had. Counting from the program's screen instead
    /// left the count anchored two dozen lines up, and the first answer after
    /// the program quit read the whole buffer as printed at once.
    fn resize(&mut self, size: Size) {
        self.vt.resize(size.cols as usize, size.rows as usize);
        if !self.scanner.on_alternate() {
            self.counted = self.trimmed + self.vt.lines().count() as u64;
        }
    }

    /// Smallest size across attached clients, or the current size when nothing
    /// is attached (the child keeps the geometry it had).
    fn effective_size(&self) -> Size {
        self.clients
            .values()
            .copied()
            .reduce(|a, b| Size::new(a.cols.min(b.cols), a.rows.min(b.rows)))
            .unwrap_or(self.size)
    }
}

/// The starting point of an attach: the screen as it stands, and everything
/// that happens from that instant on.
///
/// The three parts come apart because the output stream is read in the same
/// `select!` that sends input, so they cannot stay behind one borrow.
pub struct Attached {
    /// Dropping this detaches.
    pub attachment: Attachment,
    /// The lines behind the screen, rendered as text, or empty when the client
    /// asked for none. Written before [`Self::repaint`] and never after: the
    /// client scrolls these into its terminal's own scrollback and paints the
    /// screen over what is left.
    pub history: String,
    /// Escape sequence that repaints the screen exactly as it is. Paint it
    /// before streaming anything live.
    pub repaint: String,
    /// Live output, subscribed at the same instant the repaint was taken so
    /// that no byte is both painted and streamed.
    pub output: broadcast::Receiver<Arc<[u8]>>,
    /// The size the session settled on, which is the smallest across all
    /// attached clients and so may be smaller than the one asked for.
    pub size: Size,
}

/// A client's handle on an attached session. Dropping it detaches.
pub struct Attachment {
    session: Arc<Session>,
    names: watch::Receiver<String>,
    id: u64,
    /// A client watching rather than working. Enforced here rather than trusted
    /// to the client: the promise is what makes `mm view` safe to point at a
    /// session somebody else is working in.
    read_only: bool,
}

impl Attachment {
    pub async fn send_input(&self, bytes: Vec<u8>) -> Result<()> {
        if self.read_only {
            return Ok(());
        }
        self.session.send_input(bytes).await
    }

    /// Reserving is cancel safe: the pump retains the bytes until it obtains
    /// a slot, while output and liveness probes keep running beside this wait.
    pub(super) async fn input_space(&self) -> Result<Permit<'_, Vec<u8>>> {
        self.session
            .input_tx
            .reserve()
            .await
            .map_err(|_| anyhow::anyhow!("the session stopped accepting input"))
    }

    /// Whether this client only watches, for the paths that answer rather than
    /// ignore: a rename is refused with a reason.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Ask for a size, and answer with the one the session took.
    ///
    /// Not always the one asked for: the smallest across every attached client
    /// wins, so a client that asked for more than somebody else's terminal has
    /// been given less. A client with a screen of its own has to be told, or it
    /// reflows its copy to a shape the session never had. A viewer never joins
    /// that negotiation, so its window changing size is nothing to do with the
    /// session's geometry.
    pub fn resize(&self, size: Size) -> Size {
        if self.read_only {
            return held(&self.session.state).size;
        }
        let mut state = held(&self.session.state);
        state.clients.insert(self.id, size.sane());
        let effective = state.effective_size();
        self.session.apply_size(&mut state, effective);
        effective
    }

    /// Whether the program is expecting pastes to be bracketed, which decides
    /// how a pasted file's path is handed to it.
    pub fn bracketed_paste(&self) -> bool {
        held(&self.session.state).scanner.bracketed_paste()
    }

    /// Replace the screen and resume output from the same instant. Queued
    /// bytes are already represented in the dump and must not be replayed.
    pub(super) fn resync(&self, output: &mut broadcast::Receiver<Arc<[u8]>>) -> String {
        let state = held(&self.session.state);
        *output = self.session.output_tx.subscribe();
        state.repaint()
    }

    /// A window of the session's history, for a client scrolling back through
    /// it on a screen the terminal keeps no scrollback for.
    ///
    /// The request is brought into this instant before it is answered: `from`
    /// counts back from the newest line as the client last heard it, and the
    /// newest line has moved by however much printed since (see
    /// [`ViewRequest::printed`]). Corrected here rather than left to the
    /// client, which can only correct the answer after it arrives, and a
    /// correction that lands after the fact is a block that does not cover the
    /// window it was asked for.
    pub fn window(&self, request: &ViewRequest) -> View {
        let mut state = held(&self.session.state);
        let total = state.vt.lines().count() as u64;
        let printed = state.printed(total);
        let asked = ViewRequest {
            from: request
                .from
                .saturating_add(proto::moved(printed, request.printed)),
            ..request.clone()
        };
        super::history::window(&state.vt, printed, &asked)
    }

    /// Every line of that history holding `needle`.
    pub fn find(&self, needle: &str) -> Found {
        let mut state = held(&self.session.state);
        let total = state.vt.lines().count() as u64;
        let printed = state.printed(total);
        super::history::find(&state.vt, printed, needle)
    }

    /// What the session is called, which is what a rename asked for from here
    /// has to be looked up by: the registry keys sessions by name, and this is
    /// the only end of the stream that knows which one it is attached to.
    pub fn name(&self) -> String {
        self.session.name()
    }

    /// Subscribed when the attachment was made, so a rename during the
    /// handshake or initial repaint remains pending until the pump reads it.
    pub(super) fn name_changes(&self) -> watch::Receiver<String> {
        self.names.clone()
    }

    pub fn exit_rx(&self) -> watch::Receiver<Option<i32>> {
        self.session.exit_rx.clone()
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.session.host_clients.fetch_sub(1, Ordering::Relaxed);
        let mut state = held(&self.session.state);
        state.clients.remove(&self.id);
        let effective = state.effective_size();
        self.session.apply_size(&mut state, effective);
    }
}

impl Session {
    /// An upload is tied to this process before it starts, so a rename or a
    /// replacement session cannot redirect its path while the bytes arrive.
    pub(super) async fn paste_file(&self, path: &Path) -> Result<()> {
        if self.has_exited() {
            bail!("the session ended before the image upload finished");
        }
        let bracketed = held(&self.state).scanner.bracketed_paste();
        let bytes = super::paste::typed(path, bracketed).into_bytes();
        self.send_input(bytes).await
    }

    async fn send_input(&self, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() > proto::MAX_FRAME {
            bail!("input exceeds the {} byte frame limit", proto::MAX_FRAME);
        }
        self.input_tx
            .send(bytes)
            .await
            .map_err(|_| anyhow::anyhow!("the session stopped accepting input"))
    }

    /// Spawn the child on a fresh PTY and start the three tasks that keep the
    /// session alive: PTY reader, PTY writer, and child reaper.
    pub fn spawn(
        spec: &SpawnSpec,
        name: String,
        events: broadcast::Sender<HostedEvent>,
        host_clients: Arc<AtomicUsize>,
    ) -> Result<Arc<Self>> {
        let size = spec.size.sane();
        let Launch { argv, label } = launch(spec);

        let (pty, pts) = pty_process::open().context("opening pty")?;
        pty.resize(size.into()).context("sizing pty")?;
        let mut final_output = File::from(
            pty.as_fd()
                .try_clone_to_owned()
                .context("retaining the pty for its final output")?,
        );

        let mut cmd = pty_process::Command::new(&argv[0]);
        cmd = cmd.args(&argv[1..]);
        // The node's own environment is whatever the service manager gave it,
        // which is close to nothing. Set what a login would, and let the login
        // shell source the user's profile for the rest.
        if let Some(user) = user::current() {
            cmd = cmd.env("SHELL", &user.shell);
            cmd = cmd.env("HOME", &user.home);
            cmd = cmd.env("USER", &user.name);
            cmd = cmd.env("LOGNAME", &user.name);
            cmd = cmd.current_dir(&user.home);
        }
        // A caller's working directory wins over the home directory above.
        if let Some(cwd) = &spec.cwd {
            cmd = cmd.current_dir(cwd);
        }
        cmd = cmd.env("TERM", "xterm-256color");
        cmd = cmd.env("MM_SESSION", &name);

        let mut child = cmd
            .spawn(pts)
            .with_context(|| format!("spawning {}", argv[0]))?;
        let pid = child.id().unwrap_or(0);

        let (input_tx, mut input_rx) = mpsc::channel(INPUT_BACKLOG);
        let (size_tx, mut sizes) = watch::channel(size);
        let (output_tx, _) = broadcast::channel(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(None);

        let session = Arc::new(Session {
            name: watch::channel(name).0,
            command: label,
            pid,
            started: SystemTime::now(),
            input_tx,
            size_tx,
            output_tx: output_tx.clone(),
            state: Mutex::new(State::new(size)),
            exit_rx,
            events,
            host_clients,
            next_client: AtomicU64::new(0),
        });

        let (mut pty_read, pty_write) = pty.into_split();

        // PTY -> screen model -> attached clients.
        let s = Arc::clone(&session);
        let reader = tokio::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match pty_read.read(&mut buf).await {
                    // EOF on the master means every slave fd is closed: the
                    // child and its whole process group are gone.
                    Ok(0) | Err(_) => break,
                    Ok(n) => s.ingest(&buf[..n]),
                }
            }
        });

        // One writer owns the fd. Resize bursts keep only their latest value,
        // so resizing or dropping an attachment never waits for input space.
        tokio::spawn(async move {
            let mut pty_write = pty_write;
            loop {
                tokio::select! {
                    biased;
                    changed = sizes.changed() => {
                        if changed.is_err() { break; }
                        let size = *sizes.borrow_and_update();
                        if let Err(e) = pty_write.resize(size.into()) {
                            debug!("resize failed: {e}");
                        }
                    }
                    bytes = input_rx.recv() => {
                        let Some(bytes) = bytes else { break };
                        if pty_write.write_all(&bytes).await.is_err() { break; }
                    }
                }
            }
        });

        // Reap the child and publish its exit.
        let s = Arc::clone(&session);
        tokio::spawn(async move {
            let code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(-1),
                Err(e) => {
                    warn!(session = %s.name(), "waiting on child failed: {e}");
                    -1
                }
            };
            reader.abort();
            if let Err(error) = reader.await
                && !error.is_cancelled()
            {
                warn!(session = %s.name(), "pty reader failed: {error}");
            }
            s.drain_final_output(&mut final_output);
            s.publish(EventKind::Exited(code));
            let _ = exit_tx.send(Some(code));
        });

        Ok(session)
    }

    /// The fd remains nonblocking after duplication. Stop once its queued
    /// output is exhausted, with a bound for a surviving descendant that keeps
    /// printing. Waiting for EOF would let that descendant hold the exit open.
    fn drain_final_output(&self, output: &mut File) {
        let mut buffer = [0u8; 16 * 1024];
        for _ in 0..16 {
            match output.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => self.ingest(&buffer[..read]),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error)
                    if error.kind() == ErrorKind::WouldBlock
                        || error.raw_os_error() == Some(libc::EIO) =>
                {
                    break;
                }
                Err(error) => {
                    debug!(session = %self.name(), "reading final pty output failed: {error}");
                    break;
                }
            }
        }
    }

    /// Feed a chunk of PTY output into the screen model and fan it out.
    ///
    /// The screen update and the broadcast happen under one lock so that a
    /// client attaching concurrently either sees a byte in its dump or in its
    /// stream, never both and never neither.
    fn ingest(&self, chunk: &[u8]) {
        let mut events = Vec::new();
        let mut state = held(&self.state);

        state.scanner.feed(chunk, |e| events.push(e));
        let text = state.decoder.decode(chunk);
        // The lines this chunk pushed out of the buffer, counted as they go:
        // `avt` hands them over once, here, and nowhere else says they existed
        // (see `State::trimmed`). Consuming the iterator is what drops them,
        // so this is the count of work already being done.
        let trimmed = state.vt.feed_str(&text).scrollback.count() as u64;
        state.trimmed += trimmed;
        state.last_activity = Instant::now();

        state.bells += events.iter().filter(|e| **e == Event::Bell).count() as u64;

        let _ = self.output_tx.send(Arc::from(chunk));
        drop(state);

        for event in events {
            self.publish(match event {
                Event::Bell => EventKind::Bell,
                Event::Title(t) => EventKind::TitleChanged(t),
                Event::Notify { title, body } => EventKind::Notify { title, body },
            });
        }
    }

    /// Put an event on the node's bus, stamped with enough context that a
    /// listener never has to ask a follow-up question to render a notification.
    pub fn publish(&self, kind: EventKind) {
        let (title, attached) = {
            let state = held(&self.state);
            (state.title(&self.command), state.clients.len())
        };
        let _ = self.events.send(HostedEvent {
            host: this_machine().to_string(),
            event: SessionEvent {
                session: self.name(),
                title,
                kind,
                attached,
                host_attached: self.host_clients.load(Ordering::Relaxed),
            },
        });
    }

    /// Attach a client.
    ///
    /// `history` is how many lines from behind the screen the client wants,
    /// which is zero for one painting on a screen of its own.
    ///
    /// `read_only` is a client watching rather than working. It stays out of
    /// the size negotiation below, because somebody looking on from a phone
    /// must not shrink the screen of whoever is typing, and it takes whatever
    /// geometry the session is already at. It still counts in `host_clients`,
    /// which is deliberate and the other way round: a person watching a session
    /// is a person present, and a bell should reach the terminal they are
    /// sitting at rather than a desktop notifier somewhere else.
    pub fn attach(self: &Arc<Self>, size: Size, history: u32, read_only: bool) -> Attached {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        self.host_clients.fetch_add(1, Ordering::Relaxed);
        let mut state = held(&self.state);

        // Subscribe under the lock so the repaint and the stream meet exactly.
        let output = self.output_tx.subscribe();
        if !read_only {
            state.clients.insert(id, size.sane());
        }
        let effective = state.effective_size();
        self.apply_size(&mut state, effective);
        state.bells = 0;
        let repaint = state.repaint();
        let history = super::history::render(&state.vt, history as usize);
        drop(state);

        Attached {
            attachment: Attachment {
                session: Arc::clone(self),
                names: self.name.subscribe(),
                id,
                read_only,
            },
            history,
            repaint,
            output,
            size: effective,
        }
    }

    /// Resize the child's terminal and the screen model together.
    fn apply_size(&self, state: &mut State, size: Size) {
        if size == state.size {
            return;
        }
        state.size = size;
        state.resize(size);
        self.size_tx.send_replace(size);
    }

    pub fn name(&self) -> String {
        self.name.borrow().clone()
    }

    /// Answer to a different name from now on. The registry does the deciding,
    /// since it owns the names and is the only place a clash can be seen; this
    /// is the session's half of what it decided.
    pub fn set_name(&self, name: &str) {
        self.name.send_replace(name.to_string());
    }

    /// Ask the child to go away. SIGHUP to the whole process group, the way a
    /// terminal closing would, so shells and their children both get it.
    pub fn kill(&self) {
        self.signal(libc::SIGHUP);
    }

    /// Stop asking, for a child still there when the grace period on the way
    /// out runs out. Otherwise it is left running against a PTY whose master
    /// has closed, where every read gives EIO and nothing will reap it.
    ///
    /// This one is the child alone, not its group, and the difference is the
    /// point: something still running after a hangup it was sent deliberately
    /// is ignoring hangups, which is what `nohup` is for. A build left running
    /// on purpose survives the node that started it. Only the process holding
    /// the terminal has to go.
    ///
    /// Safe from the pid-reuse this would otherwise invite, because it is only
    /// reached while the child is unreaped: [`Session::has_exited`] is false
    /// until the reaper has waited on it, and until then the pid is still this
    /// child's.
    pub fn kill_now(&self) {
        if self.pid == 0 {
            return;
        }
        // SAFETY: kill on an unreaped child of ours; a failure is not fatal.
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
        }
    }

    fn signal(&self, signal: libc::c_int) {
        if self.pid == 0 {
            return;
        }
        // The child is a session leader (pty-process makes it one), so its pid
        // is also its process-group id. The group rather than the pid, because
        // the kernel's own hangup on the last close goes to the terminal's
        // *foreground* group, which is whatever the shell last put there and
        // not necessarily the shell itself.
        // SAFETY: killpg on a pid we spawned; a failure here is not fatal.
        unsafe {
            libc::killpg(self.pid as libc::pid_t, signal);
        }
    }

    pub fn has_exited(&self) -> bool {
        self.exit_rx.borrow().is_some()
    }

    pub fn exit_rx(&self) -> watch::Receiver<Option<i32>> {
        self.exit_rx.clone()
    }

    pub fn info(&self) -> SessionInfo {
        let state = held(&self.state);
        SessionInfo {
            name: self.name(),
            title: state.title(&self.command),
            command: self.command.clone(),
            pid: self.pid,
            size: state.size,
            attached: state.clients.len(),
            idle: state.last_activity.elapsed().as_secs(),
            bells: state.bells,
            started: self.started,
            // The same on every row, and the listing has nowhere else to put
            // it: see the field.
            node: crate::hosts::node_id().unwrap_or_default(),
        }
    }

    /// The screen as it stands, for somebody drawing a picture of it.
    ///
    /// Apart from [`Session::attach`] because taking it must not make this a
    /// session anybody is in: nothing is added to `clients`, so the geometry
    /// is left alone and `host_clients` is untouched, which is what keeps a
    /// wall of thumbnails from telling the machine that somebody is present.
    /// `size` is what the screen is painted at rather than what the caller
    /// would like, the dump having no way to reflow.
    pub fn peek(&self) -> Peek {
        let state = held(&self.state);
        Peek {
            name: self.name(),
            size: state.size,
            screen: state.vt.dump(),
        }
    }

    /// Where this session is and what is running in it, for a checkpoint.
    ///
    /// Apart from [`Self::info`] and off the listing path on purpose: this
    /// walks `/proc` per session, and a listing is asked for on every keypress
    /// the popup takes. What it reads is the operating system's rather than
    /// ours, so the only lock it takes is the name's, after the walk and for
    /// as long as a clone.
    pub fn doing(&self) -> Doing {
        let front = crate::foreground::of(self.pid);
        Doing::new(self.name(), self.pid, front)
    }
}

/// What to exec, and what to call it in a listing. The two differ because a
/// command runs *through* the login shell, and a listing should show the
/// command rather than the wrapper.
struct Launch {
    argv: Vec<String>,
    label: String,
}

/// Everything runs under the user's login shell, the way `ssh host cmd` does.
///
/// With no command that is simply an interactive login. With one, the shell
/// still starts as a login shell, so the session gets the `PATH` and the
/// environment the user's own profile sets up. A node started as a service has
/// neither, so exec'ing the command directly would fail to find half the
/// programs worth running.
fn launch(spec: &SpawnSpec) -> Launch {
    let shell = user::shell();
    // A caller that says what this is called is answering for the row in a
    // listing, which is not always the command: a restored session runs a
    // wrapper, and naming the wrapper describes the machinery rather than the
    // work.
    let named = |argv: &[String]| spec.label.clone().unwrap_or_else(|| describe(argv));
    if spec.command.is_empty() {
        return Launch {
            label: named(slice::from_ref(&shell)),
            argv: vec![shell, "-l".to_string()],
        };
    }
    Launch {
        label: named(&spec.command),
        argv: vec![shell, "-lc".to_string(), shell::join(&spec.command)],
    }
}

/// A short human label for a command, used as the default session name and as
/// the fallback title.
fn describe(argv: &[String]) -> String {
    let program = argv
        .first()
        .map(|p| {
            Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
        })
        .unwrap_or_default();
    let program = program.trim_start_matches('-').to_string();
    if argv.len() > 1 {
        format!("{program} {}", argv[1..].join(" "))
    } else {
        program
    }
}

/// Default session name for a spec: the program's basename.
pub fn default_name(spec: &SpawnSpec) -> String {
    match spec.command.first() {
        Some(program) => describe(slice::from_ref(program)),
        None => describe(&[user::shell()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::io::Result as IoResult;
    use std::pin::Pin;
    use std::task::{Context as TaskContext, Poll, Waker};
    use std::time::Duration;
    use tokio::io::{AsyncRead, ReadBuf, duplex};

    use crate::node::registry::Registry;
    use crate::proto::{FrameReader, tag};

    fn quiet_session(exit_rx: watch::Receiver<Option<i32>>) -> Arc<Session> {
        let (input_tx, _input_rx) = mpsc::channel(INPUT_BACKLOG);
        let (output_tx, _) = broadcast::channel(OUTPUT_BACKLOG);
        let (events, _) = broadcast::channel(16);
        Arc::new(Session {
            name: watch::channel("build".to_string()).0,
            command: "shell".into(),
            pid: 0,
            started: SystemTime::now(),
            input_tx,
            size_tx: watch::channel(Size::new(80, 24)).0,
            output_tx,
            state: Mutex::new(State::new(Size::new(80, 24))),
            exit_rx,
            events,
            host_clients: Arc::new(AtomicUsize::new(0)),
            next_client: AtomicU64::new(0),
        })
    }

    #[tokio::test]
    async fn stalled_input_applies_backpressure_and_keeps_bytes_in_order() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let mut session = quiet_session(exit_rx);
        let (input_tx, mut input_rx) = mpsc::channel(INPUT_BACKLOG);
        Arc::get_mut(&mut session).unwrap().input_tx = input_tx;
        let attached = session.attach(Size::default(), 0, false);
        for byte in 0..16u8 {
            attached.attachment.send_input(vec![byte]).await.unwrap();
        }
        let pending = attached.attachment.send_input(vec![16]);
        tokio::pin!(pending);
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(
            pending.as_mut().poll(&mut context).is_pending(),
            "a full input queue must stop accepting more bytes"
        );
        let first = input_rx.recv().await.expect("the first input stays queued");
        assert_eq!(first, [0]);
        pending.await.unwrap();
        for expected in 1..=16u8 {
            let bytes = input_rx.recv().await.expect("queued input stays available");
            assert_eq!(bytes, [expected]);
        }
    }

    #[tokio::test]
    async fn full_input_does_not_block_viewers_resizes_or_detach() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let mut session = quiet_session(exit_rx);
        let (input_tx, input_rx) = mpsc::channel(INPUT_BACKLOG);
        Arc::get_mut(&mut session).unwrap().input_tx = input_tx;
        let working = session.attach(Size::default(), 0, false);
        for _ in 0..INPUT_BACKLOG {
            working.attachment.send_input(vec![1]).await.unwrap();
        }
        let watching = session.attach(Size::new(10, 5), 0, true);
        let viewed = watching.attachment.send_input(vec![2]);
        tokio::pin!(viewed);
        let mut context = TaskContext::from_waker(Waker::noop());
        assert!(matches!(
            viewed.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(input_rx.len(), INPUT_BACKLOG, "a viewer enqueues no input");

        let mut sizes = session.size_tx.subscribe();
        let smaller = session.attach(Size::new(40, 12), 0, false);
        working.attachment.resize(Size::new(120, 40));
        drop(smaller);
        let last_size = *sizes.borrow_and_update();
        assert_eq!(last_size, Size::new(120, 40));
        assert_eq!(session.info().size, last_size);
        assert!(
            !sizes.has_changed().unwrap(),
            "resize bursts keep one latest size"
        );
    }

    #[tokio::test]
    async fn library_input_cannot_bypass_the_frame_size_bound() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let session = quiet_session(exit_rx);
        let error = session
            .send_input(vec![0; proto::MAX_FRAME + 1])
            .await
            .unwrap_err();
        assert!(error.to_string().contains("frame limit"));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_input_keeps_a_responsive_client_connected() {
        struct TrackingRead<R> {
            inner: R,
            total: usize,
            count: watch::Sender<usize>,
        }
        impl<R: AsyncRead + Unpin> AsyncRead for TrackingRead<R> {
            fn poll_read(
                self: Pin<&mut Self>,
                context: &mut TaskContext<'_>,
                buffer: &mut ReadBuf<'_>,
            ) -> Poll<IoResult<()>> {
                let this = self.get_mut();
                let before = buffer.filled().len();
                let result = Pin::new(&mut this.inner).poll_read(context, buffer);
                if matches!(result, Poll::Ready(Ok(()))) {
                    this.total += buffer.filled().len() - before;
                    this.count.send_replace(this.total);
                }
                result
            }
        }

        let (_exit_tx, exit_rx) = watch::channel(None);
        let mut session = quiet_session(exit_rx);
        let (input_tx, mut input_rx) = mpsc::channel(INPUT_BACKLOG);
        Arc::get_mut(&mut session).unwrap().input_tx = input_tx;
        let attached = session.attach(Size::default(), 0, false);
        for _ in 0..INPUT_BACKLOG {
            attached.attachment.send_input(vec![0]).await.unwrap();
        }
        let (client, server) = duplex(64 * 1024);
        let (read, mut write) = tokio::io::split(server);
        let (count, mut read_count) = watch::channel(0);
        let events = session.events.clone();
        let registry = Registry::new(events.clone());
        let task = tokio::spawn(async move {
            super::super::pump_attachment(
                attached,
                &registry,
                events.subscribe(),
                &mut FrameReader::new(TrackingRead {
                    inner: read,
                    total: 0,
                    count,
                }),
                &mut write,
            )
            .await
        });
        let (read, mut write) = tokio::io::split(client);
        let mut frames = FrameReader::new(read);
        proto::write_frame(&mut write, tag::PONG, &[])
            .await
            .unwrap();
        proto::write_msg(&mut write, tag::RESIZE, &Size::default())
            .await
            .unwrap();
        while frames.next().await.unwrap().unwrap().tag != tag::SIZE {}
        let before = *read_count.borrow();
        proto::write_frame(&mut write, tag::DATA, &[1])
            .await
            .unwrap();
        read_count
            .wait_for(|count| *count >= before + 6)
            .await
            .unwrap();

        for _ in 0..4 {
            tokio::time::advance(proto::PING_EVERY).await;
            let frame = frames
                .next()
                .await
                .unwrap()
                .expect("a backpressured client stays connected");
            assert_eq!(frame.tag, tag::PING);
            proto::write_frame(&mut write, tag::PONG, &[])
                .await
                .unwrap();
        }
        assert_eq!(input_rx.recv().await.unwrap(), [0]);
        proto::write_msg(&mut write, tag::RESIZE, &Size::new(100, 30))
            .await
            .unwrap();
        while frames.next().await.unwrap().unwrap().tag != tag::SIZE {}
        proto::write_frame(&mut write, tag::DETACH, &[])
            .await
            .unwrap();
        assert!(!task.await.unwrap().unwrap());
    }

    #[test]
    fn a_requested_snapshot_replaces_queued_output() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let session = quiet_session(exit_rx);
        let mut attached = session.attach(Size::new(80, 24), 0, false);
        session.ingest(b"before");
        let screen = attached.attachment.resync(&mut attached.output);
        assert!(screen.contains("before"));
        assert!(matches!(
            attached.output.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        session.ingest(b"after");
        assert_eq!(&*attached.output.try_recv().unwrap(), b"after");
    }

    #[tokio::test]
    async fn a_lagged_client_gets_one_snapshot_then_only_new_output() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let session = quiet_session(exit_rx);
        let attached = session.attach(Size::new(80, 24), 0, false);
        for _ in 0..OUTPUT_BACKLOG * 2 {
            session.ingest(b"old\r\n");
        }
        let (client, server) = duplex(64 * 1024);
        let (read, mut write) = tokio::io::split(server);
        let events = session.events.clone();
        let registry = Registry::new(events.clone());
        let task = tokio::spawn(async move {
            super::super::pump_attachment(
                attached,
                &registry,
                events.subscribe(),
                &mut FrameReader::new(read),
                &mut write,
            )
            .await
        });
        let (read, mut write) = tokio::io::split(client);
        let mut frames = FrameReader::new(read);
        assert_eq!(frames.next().await.unwrap().unwrap().tag, tag::DATA);
        let recovered = frames.next().await.unwrap().unwrap();
        assert_eq!(recovered.tag, tag::RESYNC, "a dump is not live output");
        assert!(String::from_utf8_lossy(&recovered.body).contains("old"));

        session.ingest(b"new");
        let next = frames.next().await.unwrap().unwrap();
        assert_eq!(next.tag, tag::DATA);
        assert_eq!(next.body, b"new", "queued output predates the snapshot");
        proto::write_frame(&mut write, tag::DETACH, &[])
            .await
            .unwrap();
        assert!(!task.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn a_final_output_drain_recovers_a_lagged_clients_screen() {
        let (_exit_tx, exit_rx) = watch::channel(None);
        let session = quiet_session(exit_rx);
        let mut attached = session.attach(Size::default(), 0, false);
        for _ in 0..OUTPUT_BACKLOG * 2 {
            session.ingest(b"old\r\n");
        }
        session.ingest(b"final output");
        let mut wire = Vec::new();
        super::super::drain_output(&attached.attachment, &mut attached.output, &mut wire)
            .await
            .unwrap();
        let frame = FrameReader::new(wire.as_slice())
            .next()
            .await
            .unwrap()
            .expect("the last screen is sent even when the client lagged");
        assert_eq!(frame.tag, tag::RESYNC);
        assert!(String::from_utf8_lossy(&frame.body).contains("final output"));
    }

    #[tokio::test]
    async fn a_session_releases_its_reader_when_a_descendant_keeps_the_terminal_open() {
        let dir = std::env::temp_dir().join(format!("manymux-reader-{}", std::process::id()));
        crate::config::ensure_private_dir(&dir).unwrap();
        let child_file = dir.join("child");
        let (events, _) = broadcast::channel(16);
        let session = Session::spawn(
            &SpawnSpec {
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "trap '' HUP; sleep 300 & echo $! > \"$1\"; echo ready; read go; printf 'final output'".into(),
                    "sh".into(),
                    child_file.to_str().unwrap().to_string(),
                ],
                ..SpawnSpec::default()
            },
            "build".to_string(),
            events,
            Arc::new(AtomicUsize::new(0)),
        )
        .unwrap();
        let retained = Arc::downgrade(&session);
        let mut attached = session.attach(Size::default(), 0, false);
        let mut seen = attached.repaint.clone();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !seen.contains("ready") {
                seen.push_str(&String::from_utf8_lossy(
                    &attached.output.recv().await.unwrap(),
                ));
            }
        })
        .await
        .unwrap();
        let descendant: i32 = std::fs::read_to_string(&child_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut exited = session.exit_rx();
        attached
            .attachment
            .send_input(b"go\n".to_vec())
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), exited.wait_for(Option::is_some))
            .await
            .unwrap()
            .unwrap();
        let final_output_received = session.peek().screen.contains("final output");
        drop(attached);
        drop(session);
        let released = retained.upgrade().is_none();
        // SAFETY: this pid names the background child created by this test.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
        std::fs::remove_dir_all(dir).unwrap();
        assert!(final_output_received, "buffered output precedes the exit");
        assert!(released, "an exited session must not retain its PTY reader");
    }

    #[test]
    fn a_bare_session_is_an_interactive_login_shell() {
        let launched = launch(&SpawnSpec::default());
        assert_eq!(launched.argv, vec![user::shell(), "-l".to_string()]);
    }

    /// The bug this closes: a node started as a service inherits no `SHELL` and
    /// almost no `PATH`, so a session would land you in `/bin/sh` unable to
    /// find half the programs worth running.
    #[test]
    fn a_command_runs_through_the_login_shell() {
        let spec = SpawnSpec {
            command: vec!["claude".into(), "--resume".into()],
            ..Default::default()
        };
        let launched = launch(&spec);
        assert_eq!(
            launched.argv,
            vec![
                user::shell(),
                "-lc".to_string(),
                "claude --resume".to_string()
            ]
        );
        // The listing shows what was asked for, not the wrapper.
        assert_eq!(launched.label, "claude --resume");
    }

    #[test]
    fn describe_uses_basename_and_strips_login_dash() {
        assert_eq!(describe(&["/bin/zsh".into(), "-l".into()]), "zsh -l");
        assert_eq!(describe(&["-zsh".into()]), "zsh");
    }

    #[test]
    fn effective_size_is_the_smallest_attached_client() {
        let mut clients = HashMap::new();
        clients.insert(1, Size::new(120, 40));
        clients.insert(2, Size::new(80, 50));
        let mut state = State::new(Size::new(80, 24));
        state.clients = clients;
        assert_eq!(state.effective_size(), Size::new(80, 40));
    }

    /// What [`Session::ingest`] does with a chunk of output, without the
    /// fan-out to clients: both halves see it, which is what keeps the scanner
    /// and the screen model saying the same thing about which screen is up.
    fn feed(state: &mut State, text: &str) {
        state.scanner.feed(text.as_bytes(), |_| {});
        state.trimmed += state.vt.feed_str(text).scrollback.count() as u64;
    }

    /// What [`Attachment::window`] asks it for.
    fn printed(state: &mut State) -> u64 {
        let total = state.vt.lines().count() as u64;
        state.printed(total)
    }

    /// A reflow is not the session printing: a narrower screen splits the
    /// lines that were wrapped and a wider one rejoins them, so the buffer
    /// comes out a different length either way. Counted, a phone attaching to
    /// a session somebody is reading would say a thousand lines had just been
    /// printed and throw their view back by a thousand.
    #[test]
    fn a_reflow_prints_nothing() {
        let mut state = State::new(Size::new(80, 24));
        for i in 0..50 {
            feed(
                &mut state,
                &format!("{i}: a line long enough to be folded in two by a narrower screen\r\n"),
            );
        }
        let before = printed(&mut state);
        assert!(before >= 50, "fifty lines and the screen: {before}");

        // A phone attaching, which is the smallest attached client and so the
        // one the session takes its size from.
        state.resize(Size::new(40, 24));
        assert!(
            state.vt.lines().count() as u64 > before,
            "the lines were folded"
        );
        assert_eq!(printed(&mut state), before, "and none of it was printed");

        // And the phone leaving again, which folds them back.
        state.resize(Size::new(80, 24));
        assert_eq!(printed(&mut state), before, "nor is undoing one");

        feed(&mut state, "one more\r\n");
        assert_eq!(printed(&mut state), before + 1, "what follows one counts");
    }

    /// The same resize, made while a full-screen program has the screen. What
    /// is reflowed then is the program's own screen, two dozen lines with no
    /// history behind it, and the session's buffer is put aside at the length
    /// it had. Counted from the program's screen, the first answer after it
    /// quit read the whole buffer as printed at once and took every view of the
    /// session to the top of it.
    #[test]
    fn a_resize_under_a_full_screen_program_prints_nothing() {
        let mut state = State::new(Size::new(80, 24));
        for i in 0..500 {
            feed(&mut state, &format!("line {i}\r\n"));
        }
        let before = printed(&mut state);

        feed(&mut state, "\x1b[?1049h");
        assert!(state.scanner.on_alternate(), "the program took the screen");
        assert_eq!(printed(&mut state), before, "and printed nothing taking it");

        // A second client attaching, which is what moves the size under one.
        state.resize(Size::new(80, 30));
        assert_eq!(printed(&mut state), before, "nor did the resize");

        feed(&mut state, "\x1b[?1049l");
        feed(&mut state, "one more\r\n");
        assert_eq!(
            printed(&mut state),
            before + 1,
            "one line, one line counted"
        );
    }

    /// The count a scrolling client anchors on counts what the session
    /// printed, and nothing else the buffer does.
    #[test]
    fn what_a_session_has_printed_does_not_go_back() {
        let mut state = State::new(Size::new(80, 24));
        state.trimmed = 40;
        assert_eq!(state.printed(1000), 1040, "the forty that scrolled off");
        assert_eq!(state.printed(1010), 1050, "ten lines printed since");

        // A full-screen program, which has the scrollback put aside and a
        // screen of its own, and then quits.
        state.scanner.feed(b"\x1b[?1049h", |_| {});
        assert_eq!(state.printed(24), 1050, "not a session unprinting");
        state.scanner.feed(b"\x1b[?1049l", |_| {});
        assert_eq!(state.printed(1010), 1050, "nor printing it all again");

        // A reset, which empties the buffer without trimming it, and then five
        // hundred lines printed after it. Counting those is what a high-water
        // mark could not do: it would have said nothing had been printed until
        // the buffer had grown back past a thousand.
        assert_eq!(state.printed(24), 1050, "a reset prints nothing");
        assert_eq!(state.printed(524), 1550, "and what follows one counts");
    }

    /// The title is the program's to set and nobody else's: renaming a session
    /// changes what it is called, not what it says it is doing.
    #[test]
    fn the_title_follows_the_program() {
        let mut state = State::new(Size::new(80, 24));
        assert_eq!(state.title("zsh"), "zsh", "the command is the fallback");

        state.scanner.feed(b"\x1b]0;from the program\x07", |_| {});
        assert_eq!(state.title("zsh"), "from the program");

        // An empty one is a program clearing its title, and the command is
        // still a better answer than nothing at all.
        state.scanner.feed(b"\x1b]0;\x07", |_| {});
        assert_eq!(state.title("zsh"), "zsh");
    }
}
