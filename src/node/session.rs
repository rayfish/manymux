//! A session: one PTY, one child process, and the screen state needed to make
//! reattaching look like you never left.
//!
//! The client is not the child's terminal. The PTY master stays open here for
//! the session's whole life, so a client going away is invisible to the child:
//! no SIGHUP, no EOF on stdin, nothing. That is the entire point of the
//! project, and it is why the PTY is owned by the server rather than proxied.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use anyhow::{Context, Result};
use avt::Vt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{debug, warn};

use super::events::{Event, Scanner, Utf8Decoder};
use crate::lock::held;
use crate::proto::{
    self, Doing, EventKind, Found, Peek, SessionEvent, SessionInfo, Size, SpawnSpec, View,
    ViewRequest,
};
use crate::shell;
use crate::user;

/// Lines of scrollback kept per session. Enough to scroll back through a long
/// build; small enough that a hundred sessions don't matter.
const SCROLLBACK: usize = 10_000;

/// Output chunks buffered per attached client before it is considered lagging.
/// A lagging client is resynced from the screen dump rather than disconnected.
const OUTPUT_BACKLOG: usize = 256;

/// Sent to the single task that owns the PTY write half. Input and resizes go
/// through one channel because both touch the same fd and must not interleave.
enum Input {
    Bytes(Vec<u8>),
    Resize(Size),
}

pub struct Session {
    /// What the session is addressed by, in `host/name` and everywhere else.
    /// Behind a lock because a rename changes it under whatever is reading it:
    /// the registry's key moves with it, but an event published from the PTY
    /// reader carries the name too.
    name: Mutex<String>,
    pub command: String,
    pub pid: u32,
    /// When this was opened. Outside the lock above because it is the one
    /// thing about a session that nothing changes, which is exactly why every
    /// listing is ordered by it.
    pub started: SystemTime,
    input_tx: mpsc::UnboundedSender<Input>,
    output_tx: broadcast::Sender<Arc<[u8]>>,
    state: Mutex<State>,
    /// Resolves to the child's exit code once it exits.
    exit_rx: watch::Receiver<Option<i32>>,
    /// The host-wide event bus. Bells and title changes go here so a laptop can
    /// hear about them with nothing attached.
    events: broadcast::Sender<SessionEvent>,
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
    /// happens, in [`Self::rebase`].
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
    fn resize(&mut self, size: Size) {
        self.vt.resize(size.cols as usize, size.rows as usize);
        self.counted = self.trimmed + self.vt.lines().count() as u64;
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
    id: u64,
    /// A client watching rather than working. Enforced here rather than trusted
    /// to the client: the promise is what makes `mm view` safe to point at a
    /// session somebody else is working in.
    read_only: bool,
}

impl Attachment {
    pub fn send_input(&self, bytes: Vec<u8>) {
        if self.read_only {
            return;
        }
        let _ = self.session.input_tx.send(Input::Bytes(bytes));
    }

    /// Whether this client only watches, for the paths that answer rather than
    /// ignore: a rename is refused with a reason.
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Update this client's requested size and renegotiate the effective size.
    ///
    /// A viewer never joins that negotiation, so its window changing size is
    /// nothing to do with the session's geometry.
    /// Ask for a size, and answer with the one the session took.
    ///
    /// Not always the one asked for: the smallest across every attached client
    /// wins, so a client that asked for more than somebody else's terminal has
    /// been given less. A client with a screen of its own has to be told, or it
    /// reflows its copy to a shape the session never had.
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

    /// Re-read the screen after a lag, so the client can repaint instead of
    /// rendering a hole.
    pub fn resync(&self) -> String {
        held(&self.session.state).repaint()
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
    /// Spawn the child on a fresh PTY and start the three tasks that keep the
    /// session alive: PTY reader, PTY writer, and child reaper.
    pub fn spawn(
        spec: &SpawnSpec,
        name: String,
        events: broadcast::Sender<SessionEvent>,
        host_clients: Arc<AtomicUsize>,
    ) -> Result<Arc<Self>> {
        let size = spec.size.sane();
        let Launch { argv, label } = launch(spec);

        let (pty, pts) = pty_process::open().context("opening pty")?;
        pty.resize(size.into()).context("sizing pty")?;

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

        let (input_tx, mut input_rx) = mpsc::unbounded_channel();
        let (output_tx, _) = broadcast::channel(OUTPUT_BACKLOG);
        let (exit_tx, exit_rx) = watch::channel(None);

        let session = Arc::new(Session {
            name: Mutex::new(name.clone()),
            command: label,
            pid,
            started: SystemTime::now(),
            input_tx,
            output_tx: output_tx.clone(),
            state: Mutex::new(State {
                vt: Vt::builder()
                    .size(size.cols as usize, size.rows as usize)
                    .scrollback_limit(SCROLLBACK)
                    .build(),
                scanner: Scanner::new(),
                decoder: Utf8Decoder::new(),
                trimmed: 0,
                counted: 0,
                printed: 0,
                bells: 0,
                last_activity: Instant::now(),
                clients: HashMap::new(),
                size,
            }),
            exit_rx,
            events,
            host_clients,
            next_client: AtomicU64::new(0),
        });

        let (mut pty_read, pty_write) = pty.into_split();

        // PTY -> screen model -> attached clients.
        let s = Arc::clone(&session);
        tokio::spawn(async move {
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

        // Attached clients -> PTY, with resizes interleaved on the same fd.
        tokio::spawn(async move {
            let mut pty_write = pty_write;
            while let Some(msg) = input_rx.recv().await {
                match msg {
                    Input::Bytes(b) => {
                        if pty_write.write_all(&b).await.is_err() {
                            break;
                        }
                    }
                    Input::Resize(size) => {
                        if let Err(e) = pty_write.resize(size.into()) {
                            debug!("resize failed: {e}");
                        }
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
            s.publish(EventKind::Exited(code));
            let _ = exit_tx.send(Some(code));
        });

        Ok(session)
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

    /// Put an event on the host-wide bus, stamped with enough context that a
    /// listener never has to ask a follow-up question to render a notification.
    pub fn publish(&self, kind: EventKind) {
        let (title, attached) = {
            let state = held(&self.state);
            (state.title(&self.command), state.clients.len())
        };
        let _ = self.events.send(SessionEvent {
            session: self.name(),
            title,
            kind,
            attached,
            host_attached: self.host_clients.load(Ordering::Relaxed),
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
        let _ = self.input_tx.send(Input::Resize(size));
    }

    pub fn name(&self) -> String {
        held(&self.name).clone()
    }

    /// Answer to a different name from now on. The registry does the deciding,
    /// since it owns the names and is the only place a clash can be seen; this
    /// is the session's half of what it decided.
    pub fn set_name(&self, name: &str) {
        *held(&self.name) = name.to_string();
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
        Doing {
            name: self.name(),
            pid: self.pid,
            cwd: front.cwd,
            foreground: front.argv,
        }
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
            label: named(std::slice::from_ref(&shell)),
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
            std::path::Path::new(p)
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
        Some(program) => describe(std::slice::from_ref(program)),
        None => describe(&[user::shell()]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let state = State {
            vt: Vt::new(80, 24),
            scanner: Scanner::new(),
            decoder: Utf8Decoder::new(),
            trimmed: 0,
            counted: 0,
            printed: 0,
            bells: 0,
            last_activity: Instant::now(),
            clients,
            size: Size::new(80, 24),
        };
        assert_eq!(state.effective_size(), Size::new(80, 40));
    }

    /// A reflow is not the session printing: a narrower screen splits the
    /// lines that were wrapped and a wider one rejoins them, so the buffer
    /// comes out a different length either way. Counted, a phone attaching to
    /// a session somebody is reading would say a thousand lines had just been
    /// printed and throw their view back by a thousand.
    #[test]
    fn a_reflow_prints_nothing() {
        let mut state = State {
            vt: Vt::builder()
                .size(80, 24)
                .scrollback_limit(SCROLLBACK)
                .build(),
            scanner: Scanner::new(),
            decoder: Utf8Decoder::new(),
            trimmed: 0,
            counted: 0,
            printed: 0,
            bells: 0,
            last_activity: Instant::now(),
            clients: HashMap::new(),
            size: Size::new(80, 24),
        };
        for i in 0..50 {
            state.vt.feed_str(&format!(
                "{i}: a line long enough to be folded in two by a narrower screen
"
            ));
        }
        let total = state.vt.lines().count() as u64;
        let before = state.printed(total);
        assert!(before >= 50, "fifty lines and the screen: {before}");

        // A phone attaching, which is the smallest attached client and so the
        // one the session takes its size from.
        state.resize(Size::new(40, 24));
        let total = state.vt.lines().count() as u64;
        assert!(total > before, "the lines were folded: {total}");
        assert_eq!(state.printed(total), before, "and none of it was printed");

        // And the phone leaving again, which folds them back.
        state.resize(Size::new(80, 24));
        let total = state.vt.lines().count() as u64;
        assert_eq!(state.printed(total), before, "nor is undoing one");

        state.vt.feed_str(
            "one more
",
        );
        let total = state.vt.lines().count() as u64;
        assert_eq!(state.printed(total), before + 1, "what follows one counts");
    }

    /// The count a scrolling client anchors on counts what the session
    /// printed, and nothing else the buffer does.
    #[test]
    fn what_a_session_has_printed_does_not_go_back() {
        let mut state = State {
            vt: Vt::new(80, 24),
            scanner: Scanner::new(),
            decoder: Utf8Decoder::new(),
            trimmed: 40,
            counted: 0,
            printed: 0,
            bells: 0,
            last_activity: Instant::now(),
            clients: HashMap::new(),
            size: Size::new(80, 24),
        };
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
        let mut state = State {
            vt: Vt::new(80, 24),
            scanner: Scanner::new(),
            decoder: Utf8Decoder::new(),
            trimmed: 0,
            counted: 0,
            printed: 0,
            bells: 0,
            last_activity: Instant::now(),
            clients: HashMap::new(),
            size: Size::new(80, 24),
        };
        assert_eq!(state.title("zsh"), "zsh", "the command is the fallback");

        state.scanner.feed(b"\x1b]0;from the program\x07", |_| {});
        assert_eq!(state.title("zsh"), "from the program");

        // An empty one is a program clearing its title, and the command is
        // still a better answer than nothing at all.
        state.scanner.feed(b"\x1b]0;\x07", |_| {});
        assert_eq!(state.title("zsh"), "zsh");
    }
}
