//! A manymux node: one per machine, per user, owning that machine's sessions.
//!
//! The node never touches the network. It listens on an owner-only Unix socket
//! and nothing else; other machines reach it by running [`agent`] over ssh,
//! which bridges its own stdin and stdout to that socket. So sshd decides who
//! gets in, using whatever the admin already configured, and manymux has no
//! second allowlist to drift out of sync with the real one.
//!
//! It also means the account question answers itself: `ssh deploy@box` lands in
//! deploy's node, because the socket lives under deploy's runtime directory.

pub mod events;
mod history;
pub mod paste;
pub mod peers;
pub mod registry;
pub mod session;

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};

use crate::client::Stream;
use crate::hosts::{Hosts, this_machine};
use crate::notify::Notifier;
use crate::proto::{
    self, FindRequest, FrameReader, HostedEvent, PING_EVERY, Renamed, Request, Response,
    SILENT_FOR, ViewRequest, tag,
};
use peers::Peers;
use registry::Registry;

/// How often to check whether the watched-hosts file changed.
const HOSTS_POLL: Duration = Duration::from_secs(2);

/// Events buffered per subscriber before it is considered too far behind.
const EVENT_BACKLOG: usize = 256;

// `PING_EVERY` and `SILENT_FOR` are imported rather than written here because
// both halves of the stream work to the same pair. A client gives up on a host
// that stops asking, so a node asking less often than the client's patience
// would have every idle session look like a dead connection. What the deadline
// buys on this side: until something says otherwise, a client that vanished
// without detaching keeps its say in the session's size and keeps counting as
// attached in `mm ls`.

/// How long a session's process group gets between the hangup and being killed
/// outright, on the way out.
///
/// Long enough for a shell to write its history and an editor to write its swap
/// file, short enough to fit inside the window [`stop`] gives the socket to go
/// quiet: overrun that and `mm stop` reports a node still listening when it is
/// only still saying goodbye.
pub const GRACE: Duration = Duration::from_secs(2);

/// What a node needs to start.
pub struct Config {
    /// Machines to watch for events, as ssh destinations.
    pub peers: Vec<String>,
    /// Watched for changes so adding a machine does not mean a restart, which
    /// would take every local session with it. `None` in tests.
    pub hosts_file: Option<PathBuf>,
    pub notifications: bool,
}

pub struct Node {
    pub registry: Arc<Registry>,
    peers: Peers,
    notifier: Arc<Notifier>,
    /// Ours and every watched machine's events. The notifier reads this, and so
    /// does anything asking for `Events` over the socket.
    events: broadcast::Sender<HostedEvent>,
    /// SHA-256 of the binary this node started from, taken before an update can
    /// replace the file. Asking again later would hash whatever now sits at
    /// that path, which is the new binary and not what this process is running.
    build: Option<String>,
}

impl Node {
    pub async fn start(config: Config) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        let node = Arc::new(Self {
            registry: Registry::new(),
            peers: Peers::default(),
            notifier: Arc::new(Notifier::new(config.notifications)),
            events,
            build: own_build().await,
        });

        // Our own sessions' events, tagged with this machine's name.
        let own = Arc::clone(&node);
        tokio::spawn(async move {
            let mut events = own.registry.subscribe();
            while let Ok(event) = events.recv().await {
                let _ = own.events.send(HostedEvent {
                    host: this_machine().to_string(),
                    event,
                });
            }
        });

        node.watch_peers(&config.peers);
        info!(peers = node.peers.len(), "node started");

        if let Some(path) = config.hosts_file {
            let watcher = Arc::clone(&node);
            tokio::spawn(async move { watcher.watch_hosts_file(path).await });
        }
        node
    }

    /// Watch exactly these machines, dropping any others.
    fn watch_peers(self: &Arc<Self>, wanted: &[String]) {
        for host in self.peers.sync(wanted) {
            self.watch_host(host);
        }
    }

    /// Re-read the watched-hosts file when it changes, so `mm add` never
    /// costs a restart. The node holding your sessions is the same one holding
    /// the subscriptions, and restarting it would kill them.
    async fn watch_hosts_file(self: Arc<Self>, path: PathBuf) {
        let mut seen = modified(&path);
        loop {
            tokio::time::sleep(HOSTS_POLL).await;
            let now = modified(&path);
            if now == seen {
                continue;
            }
            seen = now;
            match Hosts::load() {
                Ok(hosts) => {
                    debug!("watched hosts changed, resyncing");
                    self.watch_peers(&hosts.names());
                }
                Err(e) => warn!("ignoring an unreadable host list: {e:#}"),
            }
        }
    }

    /// Subscribe to one machine's events until it stops being reachable.
    ///
    /// Returning is giving up, and nothing on a timer undoes it: a machine that
    /// has been unreachable for a minute is usually one that will be
    /// unreachable for the afternoon, and the daemon has no way to tell the
    /// difference between a laptop lid and a dead host. What does know is the
    /// person: reaching the machine from a client resubscribes it
    /// ([`Node::reached`]).
    async fn watch(&self, host: String) {
        let mut failed = 0;
        loop {
            let since = Instant::now();
            if let Err(e) = self.subscribe_once(&host).await {
                debug!(host = %host, "event subscription ended: {e:#}");
            }
            // A subscription that stood up for a while is the machine having
            // been reachable, whatever ended it, so the count starts over. One
            // that took the connection and dropped it is not, or a host that
            // accepts ssh and hangs up would be retried every five seconds for
            // as long as it kept doing it.
            if since.elapsed() >= peers::STEADY {
                failed = 0;
            }
            failed += 1;
            let Some(delay) = peers::retry_after(failed) else {
                info!(host = %host, "unreachable, so no longer watching it: `mm ls` picks it back up");
                return;
            };
            tokio::time::sleep(delay).await;
        }
    }

    /// Start watching a machine a client just reached, if it is one we watch
    /// and gave up on.
    ///
    /// This is the other half of [`Node::watch`] giving up. The node cannot see
    /// the client's ssh at all, since a client talks to a remote machine
    /// itself rather than through the node here, so being told is the only way
    /// it finds out that the network is worth trying again.
    pub fn rewatch(self: &Arc<Self>, answered: &[String], watched: &[String]) {
        for host in answered {
            // Only machines on the list. A client reaches plenty of machines
            // nobody asked to hear a bell from, and an ssh destination typed
            // once is not `mm add`.
            if !watched.contains(host) || self.peers.watched(host) {
                continue;
            }
            info!(host = %host, "reachable again, subscribing to its events");
            self.watch_host(host.clone());
        }
    }

    /// Spawn the task that keeps one machine's subscription up.
    fn watch_host(self: &Arc<Self>, host: String) {
        let node = Arc::clone(self);
        let watching = host.clone();
        let task = tokio::spawn(async move { node.watch(watching).await });
        self.peers.watching(host, task);
    }

    async fn subscribe_once(&self, host: &str) -> Result<()> {
        // No consent: this reconnects in a loop with nobody watching, and a
        // machine the daemon cannot reach is one to keep trying, not one to
        // start putting software on.
        let mut stream = Stream::over_ssh(host, None).await?;
        match stream.call(&Request::Events).await? {
            Response::Ok => {}
            other => bail!("unexpected response to events: {other:?}"),
        }
        debug!(host = %host, "subscribed to events");

        // A machine calls its own sessions by its own hostname; from here they
        // are the name we reach it by, which is what the user typed.
        while let Some(mut hosted) = stream.next_event::<HostedEvent>().await? {
            hosted.host = host.to_string();
            self.notifier.handle(&hosted.host, &hosted.event).await;
            // Ignore the error: nobody watching is the normal case.
            let _ = self.events.send(hosted);
        }
        Ok(())
    }

    /// Serve this machine's owner on a Unix socket.
    ///
    /// The socket is 0600 in a per-user runtime directory, so reaching it means
    /// already being this user. There is nothing further to authenticate.
    pub async fn serve(self: Arc<Self>, socket: &Path) -> Result<()> {
        crate::ipc::serve(socket, move |read, write| {
            let node = Arc::clone(&self);
            async move { node.handle(read, write).await }
        })
        .await
    }

    /// End every session on this machine, as gently as a process on its way out
    /// can manage.
    ///
    /// Exiting alone would end them anyway: closing the last fd on a PTY master
    /// hangs the terminal up. But it ends them badly, in two ways. The kernel
    /// sends that hangup to the terminal's *foreground* process group, which is
    /// whatever the shell last put there: a shell sitting behind a full-screen
    /// program is not in it, and learns its terminal is gone only by reading
    /// EIO, long past the point where it would have written its history. And
    /// nothing waits for any of it, since the node is already gone.
    ///
    /// So: hang up each session's own group deliberately, the way `mm kill`
    /// does, wait for the children to be reaped, and kill what is still there
    /// when the grace runs out rather than leave it on a dead terminal.
    pub async fn shutdown(&self, grace: Duration) {
        let sessions = self.registry.all();
        if sessions.is_empty() {
            return;
        }
        info!(sessions = sessions.len(), "hanging up every session");
        for session in &sessions {
            session.kill();
        }

        // One deadline across all of them rather than one each: they were hung
        // up together, so they are going away together, and waiting in turn
        // costs nothing.
        let deadline = Instant::now() + grace;
        for session in &sessions {
            let mut exited = session.exit_rx();
            let left = deadline.saturating_duration_since(Instant::now());
            let _ = timeout(left, exited.wait_for(|code| code.is_some())).await;
        }

        for session in &sessions {
            if !session.has_exited() {
                warn!(session = %session.name(), "still there after the hangup; killing it");
                session.kill_now();
            }
        }
    }

    /// Serve one stream: a single request, then either a response or, for
    /// `Attach` and `Events`, something lasting until the client goes away.
    pub async fn handle<R, W>(self: &Arc<Self>, read: R, mut write: W) -> Result<()>
    where
        R: AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut read = FrameReader::new(read);
        let Some(frame) = read.next().await? else {
            return Ok(());
        };
        if frame.tag != tag::REQUEST {
            bail!("expected a request, got tag {:#x}", frame.tag);
        }

        // A machine that is behind gets requests it has never heard of, which is
        // normal in a fleet updated one host at a time. Say so, rather than
        // hanging up and leaving the caller to report a closed connection.
        let request = match proto::decode::<Request>(&frame.body) {
            Ok(request) => request,
            Err(e) => {
                debug!("undecodable request: {e:#}");
                let complaint = anyhow::anyhow!(
                    "this machine runs manymux {}, which does not understand that request: \
                     update it",
                    crate::VERSION
                );
                return reply(&mut write, Err(complaint)).await;
            }
        };

        match request {
            Request::Attach {
                name,
                size,
                history,
                read_only,
            } => {
                let Some(session) = self.registry.get(&name) else {
                    return reply(&mut write, Err(anyhow::anyhow!("no session named {name}")))
                        .await;
                };
                let attached = session.attach(size, history, read_only);
                let response = Response::Attached {
                    size: attached.size,
                    paste: true,
                    scroll: true,
                    rename: true,
                    events: true,
                    read_only: true,
                };
                proto::write_msg(&mut write, tag::RESPONSE, &response).await?;
                pump_attachment(
                    attached,
                    &self.registry,
                    self.events.subscribe(),
                    read,
                    write,
                )
                .await
            }

            Request::Events => {
                proto::write_msg(&mut write, tag::RESPONSE, &Response::Ok).await?;
                crate::ipc::pump_events(self.events.subscribe(), read, write).await
            }

            // Answer before going, so the caller learns it worked rather than
            // watching the socket vanish and having to guess why.
            Request::Stop => {
                proto::write_msg(&mut write, tag::RESPONSE, &Response::Ok).await?;
                info!(sessions = self.registry.list().len(), "stopping on request");
                self.shutdown(GRACE).await;
                // The reply is in the socket buffer, not yet read. A moment for
                // the caller to take it costs nothing and saves a confusing
                // "connection reset" on the way out. Still needed after the
                // shutdown above, which returns at once when there is nothing
                // to hang up.
                tokio::time::sleep(Duration::from_millis(50)).await;
                std::process::exit(0);
            }

            // Answered rather than acted on and forgotten, so a client can tell
            // an old node (which refuses the request) from a new one.
            Request::Reached { hosts } => {
                let watched = Hosts::load().map(|hosts| hosts.names()).unwrap_or_default();
                self.rewatch(&hosts, &watched);
                reply(&mut write, Ok(Response::Ok)).await
            }

            request => reply(&mut write, self.answer(request)).await,
        }
    }

    /// Everything answered with a single response.
    fn answer(&self, request: Request) -> Result<Response> {
        match request {
            Request::List => Ok(Response::Sessions(self.registry.list())),
            Request::Spawn(spec) => self.registry.spawn(&spec).map(|session| Response::Spawned {
                name: session.name(),
            }),
            Request::Kill { name } => self.registry.kill(&name).map(|()| Response::Ok),
            Request::Rename { name, to } => self.registry.rename(&name, &to).map(|_| Response::Ok),
            Request::Version => Ok(Response::Version {
                version: crate::VERSION.to_string(),
                build: self.build.clone(),
            }),
            Request::Snapshot => Ok(Response::Snapshot(self.registry.doing())),
            Request::Attach { .. } | Request::Events | Request::Stop | Request::Reached { .. } => {
                unreachable!("handled before the single-response path")
            }
        }
    }

    pub fn watched(&self) -> Vec<String> {
        self.peers.names()
    }
}

/// Send a response, turning a failure into one the client can print.
async fn reply<W>(write: &mut W, response: Result<Response>) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let response = response.unwrap_or_else(|e| Response::Error(format!("{e:#}")));
    proto::write_msg(write, tag::RESPONSE, &response).await
}

/// SHA-256 of the binary this process is running, or `None` if it cannot be
/// read. A node that cannot say what it is running is no reason not to start
/// one: it costs an update the chance to notice a stale node, and nothing else.
async fn own_build() -> Option<String> {
    let binary = std::env::current_exe().ok()?;
    match crate::update::sha256_of(&binary).await {
        Ok(digest) => Some(digest),
        Err(e) => {
            debug!("cannot checksum {}: {e:#}", binary.display());
            None
        }
    }
}

/// A file's modification time, or `None` if it is missing or unreadable. Both
/// count as "no change" until it appears.
fn modified(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

/// Move bytes between an attached client and its session until one end stops.
///
/// Whatever ends this loop, the child is untouched: dropping the attachment
/// only removes the client from the session's size negotiation.
///
/// The stream also carries this machine's other sessions' events, unasked. A
/// client sitting in one session is the nearest thing there is to a person on
/// this machine, so a bell in the session next door goes to their terminal
/// rather than to a desktop notifier that may be a continent away. It costs an
/// older client nothing: it skips a tag it does not know.
async fn pump_attachment<R, W>(
    attached: session::Attached,
    registry: &Registry,
    mut events: broadcast::Receiver<HostedEvent>,
    mut read: FrameReader<R>,
    mut write: W,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let session::Attached {
        attachment,
        history,
        repaint,
        mut output,
        size: _,
    } = attached;
    // Before the screen: the client scrolls these into its terminal's own
    // scrollback and paints the screen over the blank they leave behind.
    // Chunked because a thousand lines is bigger than a frame.
    for chunk in history.as_bytes().chunks(proto::PASTE_CHUNK) {
        send(&mut write, tag::HISTORY, chunk).await?;
    }
    // Paint the screen as it stands before streaming anything live.
    send(&mut write, tag::DATA, repaint.as_bytes()).await?;

    let mut exit_rx = attachment.exit_rx();
    // A whole interval before the first probe: a client that just attached has
    // nothing to prove yet.
    let start = tokio::time::Instant::now();
    let mut ping = tokio::time::interval_at(start + PING_EVERY, PING_EVERY);
    let mut last_heard = start;
    // Only a client that has answered a ping is held to the deadline, so an
    // older one, which skips the tag it does not know, keeps working as before.
    let mut answers_pings = false;
    let mut incoming = Paste::default();
    // Cleared if the bus ever closes, so a closed channel returning at once
    // cannot turn this loop into a spin.
    let mut watching = true;

    loop {
        tokio::select! {
            frame = read.next() => {
                let Some(frame) = frame? else { break };
                last_heard = tokio::time::Instant::now();
                match frame.tag {
                    tag::DATA => attachment.send_input(frame.body),
                    tag::RESIZE => attachment.resize(proto::decode(&frame.body)?),
                    tag::PONG => answers_pings = true,
                    // The client swallowed a screen switch, so what the
                    // terminal shows is no longer what the session thinks it
                    // is showing. This model is the only place the right
                    // picture still exists.
                    tag::RESYNC => {
                        send(&mut write, tag::RESYNC, attachment.resync().as_bytes()).await?
                    }
                    // A client scrolling back through what the session printed.
                    // Answered from the model, which is the only place those
                    // lines still exist: the terminal a client on a screen of
                    // its own is sitting at keeps no scrollback for that screen.
                    tag::VIEW => {
                        let request: ViewRequest = proto::decode(&frame.body)?;
                        let view = attachment.window(&request);
                        proto::write_msg(&mut write, tag::VIEW, &view).await?;
                    }
                    // A search through the same lines. Every hit at once: they
                    // are already in memory here, and walking them one round
                    // trip at a time is what makes a search on a machine two
                    // hops away feel like a search on a bad website.
                    tag::FIND => {
                        let request: FindRequest = proto::decode(&frame.body)?;
                        let found = attachment.find(&request.needle);
                        proto::write_msg(&mut write, tag::FIND, &found).await?;
                    }
                    // A name typed at the client's own prompt, which is the
                    // same rename `mm rename` asks for from outside. Answered,
                    // unlike a resize: the registry can refuse it, and the
                    // client is showing the old name until it hears otherwise.
                    tag::RENAME => {
                        let wanted: String = proto::decode(&frame.body)?;
                        // A viewer is refused with a reason rather than
                        // ignored, this being the one thing it can ask for that
                        // has an answer to give back.
                        let answer = if attachment.read_only() {
                            Renamed::Refused("watching, so nothing here can be changed".into())
                        } else {
                            match registry.rename(&attachment.name(), &wanted) {
                                Ok(name) => Renamed::Name(name),
                                Err(e) => {
                                    debug!("refusing a rename from an attached client: {e:#}");
                                    Renamed::Refused(format!("{e:#}"))
                                }
                            }
                        };
                        proto::write_msg(&mut write, tag::RENAME, &answer).await?;
                    }
                    tag::PASTE => incoming.take(frame.body),
                    tag::PASTE_END => {
                        let info: proto::PasteInfo = proto::decode(&frame.body)?;
                        // A paste that fails is reported to the log and not to
                        // the client: there is no way to say anything on this
                        // stream that would not land in the middle of whatever
                        // the program is drawing.
                        match incoming.finish(&info.kind) {
                            Ok(path) => {
                                debug!(path = %path.display(), "pasted a file into the session");
                                let typed = paste::typed(&path, attachment.bracketed_paste());
                                attachment.send_input(typed.into_bytes());
                            }
                            Err(e) => warn!("dropping a paste: {e:#}"),
                        }
                    }
                    tag::DETACH => break,
                    other => warn!("ignoring unexpected tag {other:#x} while attached"),
                }
            }
            chunk = output.recv() => match chunk {
                Ok(bytes) => send(&mut write, tag::DATA, &bytes).await?,
                // The client fell behind. Repaint from the current screen
                // instead of showing it a gap.
                Err(RecvError::Lagged(n)) => {
                    debug!("client lagged {n} chunks, resyncing");
                    send(&mut write, tag::DATA, attachment.resync().as_bytes()).await?;
                }
                Err(RecvError::Closed) => break,
            },
            event = events.recv(), if watching => match event {
                // This machine's own sessions only, and never the one already
                // on the screen. What a watched peer is doing is the desktop
                // notifier's business, not this terminal's.
                //
                // Asked of the session rather than remembered from the attach:
                // a rename since would leave this comparing against a name
                // nothing is called any more, and the client would hear its own
                // bells as the session next door's.
                Ok(hosted)
                    if hosted.host == this_machine()
                        && hosted.event.session != attachment.name() =>
                {
                    send(&mut write, tag::EVENT, &proto::encode(&hosted)?).await?;
                }
                Ok(_) => {}
                // A bell that arrived while the client was busy is not worth
                // catching up on: it rang, and the moment has passed.
                Err(RecvError::Lagged(n)) => debug!("client missed {n} events"),
                Err(RecvError::Closed) => watching = false,
            },
            _ = ping.tick() => {
                if answers_pings && last_heard.elapsed() > SILENT_FOR {
                    bail!("client stopped answering {:?} ago", last_heard.elapsed());
                }
                send(&mut write, tag::PING, &[]).await?;
            }
            _ = exit_rx.changed() => {
                let Some(code) = *exit_rx.borrow() else { continue };
                // The child's last words are still in flight: the reader task
                // may not have drained the PTY yet, and select! could have
                // picked this branch over a ready chunk. Flush both before
                // telling the client it is over.
                drain_output(&mut output, &mut write).await?;
                send(&mut write, tag::EXIT, &proto::encode(&code)?).await?;
                break;
            }
        }
    }
    Ok(())
}

/// A pasted file arriving a frame at a time.
///
/// Held per attachment rather than per session: a paste belongs to the client
/// sending it, and two clients pasting at once must not interleave into one
/// corrupt file.
#[derive(Default)]
struct Paste {
    data: Vec<u8>,
    /// Set when the file went over the limit. The chunks after it are still
    /// read and thrown away, so the stream stays in step, and the end of it is
    /// refused rather than written down half a file.
    too_big: bool,
}

impl Paste {
    fn take(&mut self, chunk: Vec<u8>) {
        if self.too_big {
            return;
        }
        if self.data.len() + chunk.len() > proto::MAX_PASTE {
            self.too_big = true;
            self.data = Vec::new();
            return;
        }
        if self.data.is_empty() {
            self.data = chunk;
        } else {
            self.data.extend_from_slice(&chunk);
        }
    }

    /// Write what has arrived, and be empty again either way: a refused paste
    /// must not turn up on the end of the next one.
    fn finish(&mut self, kind: &str) -> Result<PathBuf> {
        let data = std::mem::take(&mut self.data);
        if std::mem::take(&mut self.too_big) {
            bail!("the file was over the {} byte limit", proto::MAX_PASTE);
        }
        if data.is_empty() {
            bail!("the paste carried no data");
        }
        paste::write(kind, &data)
    }
}

/// Write a frame to an attached client, giving up on one that has stopped
/// reading.
///
/// The deadline is what makes the ping above work at all. A client whose
/// connection is dead but not closed leaves its ssh channel's window full, and
/// a write into a full window blocks rather than failing. Without this the loop
/// would park on that write forever, never reach the ping arm, and hold the
/// attachment open for the life of the process.
async fn send<W>(write: &mut W, tag: u8, body: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    match tokio::time::timeout(SILENT_FOR, proto::write_frame(write, tag, body)).await {
        Ok(written) => written,
        Err(_) => bail!("client stopped reading for {SILENT_FOR:?}"),
    }
}

/// Write every chunk already queued for this client, so nothing that landed
/// during the handover is lost.
async fn drain_output<W>(output: &mut broadcast::Receiver<Arc<[u8]>>, write: &mut W) -> Result<()>
where
    W: AsyncWrite + Unpin + Send,
{
    while let Ok(bytes) = output.try_recv() {
        send(write, tag::DATA, &bytes).await?;
    }
    Ok(())
}

/// How long to wait for a node we just started to come up.
const START_TIMEOUT: Duration = Duration::from_secs(10);

/// Bridge this process's stdin and stdout to the node on this machine, starting
/// one if it is not running.
///
/// This is what `ssh <host> mm agent` runs, and it is the only way in from
/// another machine. Starting the node on demand means a remote box needs no
/// service installed and no setup beyond having the binary, the way `tmux`
/// starts its server the first time you ask for a session.
///
/// The two directions are raced rather than joined, because either one ending
/// means the relay is over. `copy_bidirectional` would wait for both, and the
/// half reading stdin never ends on its own: `tokio::io::stdin` reads on a
/// blocking thread that cannot be cancelled, so an agent whose node has hung up
/// would sit there holding a runtime and a dead socket until ssh closed the
/// channel. That left one behind for every remote command.
pub async fn agent(socket: &Path) -> Result<()> {
    ensure_running(socket).await?;
    let node = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;
    let (mut from_node, mut to_node) = node.into_split();

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let out = async {
        let copied = tokio::io::copy(&mut from_node, &mut stdout).await;
        // The last frame is usually the one saying the session is over, and a
        // client still waiting for it is the hang this whole path is about.
        stdout.flush().await?;
        copied
    };
    tokio::select! {
        relayed = out => { relayed.context("relaying from the node to ssh")?; }
        relayed = tokio::io::copy(&mut stdin, &mut to_node) => {
            relayed.context("relaying from ssh to the node")?;
        }
    }
    Ok(())
}

/// Make sure this machine has a node running, starting one if not.
///
/// Cheap and idempotent when one is already up, which is the common case.
pub async fn ensure_running(socket: &Path) -> Result<()> {
    if tokio::net::UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }
    start_node(socket).await
}

/// Stop the node on this machine, and wait for it to actually be gone.
///
/// Every session it owns dies with it: they are its children, holding PTYs it
/// owns. Nothing here asks whether that is wanted, so the caller has to.
pub async fn stop(socket: &Path) -> Result<()> {
    // A node older than this binary has never heard of `Stop`: it fails to
    // decode the request and hangs up, so asking politely reports nothing more
    // useful than a closed connection. It still has to go, or a new binary
    // lands on the next reboot and not before, so fall back to the pid holding
    // the socket. Every machine gets this once, on the update that reaches it.
    if let Err(e) = ask_to_stop(socket).await {
        debug!("the node did not take a stop request: {e:#}");
        signal_node(socket)
            .await
            .context("stopping a node that did not answer a stop request")?;
    }

    // Waiting matters: whatever starts a node next has to bind the socket
    // rather than lose a race with the one on its way out.
    for _ in 0..100 {
        if tokio::net::UnixStream::connect(socket).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the node was asked to stop but is still listening on {socket:?}")
}

/// Stop the node and start one again, which is how a new binary is picked up.
pub async fn restart(socket: &Path) -> Result<()> {
    stop(socket).await?;
    // A service manager will have started one already; this covers a node that
    // was started on demand.
    ensure_running(socket).await
}

async fn ask_to_stop(socket: &Path) -> Result<()> {
    let mut stream = Stream::local(socket)
        .await
        .context("connecting to the node")?;
    stream.call(&Request::Stop).await?;
    Ok(())
}

/// Terminate the node by the pid the kernel reports for its socket.
///
/// SIGTERM rather than SIGKILL: the difference to the sessions is nil (they die
/// with the node either way, being its children), but the node still gets to
/// flush its log.
async fn signal_node(socket: &Path) -> Result<()> {
    let stream = tokio::net::UnixStream::connect(socket)
        .await
        .context("connecting to the node")?;
    let pid = crate::ipc::peer_pid(&stream).context("asking the kernel who holds the socket")?;
    drop(stream);

    // SAFETY: kill touches no memory. A pid that has already exited gives ESRCH,
    // which is the outcome we wanted anyway.
    let sent = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if sent != 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(libc::ESRCH) {
            bail!("signalling the node (pid {pid}): {e}");
        }
    }
    Ok(())
}

/// Start a node in the background and wait for its socket to appear.
///
/// The node has to leave the client behind completely, because from here on it
/// owns every session on the machine and the client is a command someone typed.
/// A child keeps its parent's process group and session, and a terminal signals
/// the whole foreground group: a Ctrl-C at the wrong moment, or the window
/// closing, would reach the node along with the client and take every session
/// down at once, without even leaving a line in the log to say so.
async fn start_node(socket: &Path) -> Result<()> {
    let binary = std::env::current_exe().context("finding the mm binary")?;
    info!("no node running, starting one");
    note_a_node_left_behind(socket).await;
    let mut command = Command::new(&binary);
    command
        .arg("--socket")
        .arg(socket)
        .arg("daemon")
        // Nothing is watching its output, and inheriting ssh's pipes would hold
        // the ssh session open for as long as the node lived.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: this runs in the forked child before the exec, where only
    // async-signal-safe calls are allowed. `setsid` is one, and it is the only
    // thing here. It cannot fail the one way it is documented to: that needs
    // the caller to already lead a process group, and a fresh child never does.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        })
    };
    command
        .spawn()
        .with_context(|| format!("starting {}", binary.display()))?;

    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("started a node but it did not come up within {START_TIMEOUT:?}")
}

/// Say so when an older build's node is still up somewhere else.
///
/// The socket used to follow `$XDG_RUNTIME_DIR`, so the first command after an
/// update can start an empty node beside one that is still running and still
/// owns every session left on this machine. Nothing here touches that node:
/// stopping it kills those sessions, and only the person looking at them can
/// decide when that is fine.
pub async fn note_a_node_left_behind(socket: &Path) {
    // A socket named with `--socket` is a second node on purpose, not a machine
    // that has just been updated.
    if socket != crate::config::socket() {
        return;
    }
    for old in crate::config::legacy_sockets() {
        if tokio::net::UnixStream::connect(&old).await.is_ok() {
            warn!(
                "a node from an older build is still listening on {old}, and \
                 the sessions it owns are still running. This build uses {new}. \
                 To get to them: `mm --socket {old} ls`, then `mm --socket \
                 {old} attach <name>`.",
                old = old.display(),
                new = socket.display(),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn watching_nothing() -> Arc<Node> {
        Node::start(Config {
            peers: Vec::new(),
            hosts_file: None,
            notifications: false,
        })
        .await
    }

    /// `.invalid` can never resolve (RFC 2606), so the watcher this starts
    /// fails at once and never touches the network.
    const NOWHERE: &str = "nowhere.invalid";

    /// The other half of a watcher giving up: without this, a machine that came
    /// back would stay unwatched until the node was restarted, and its bells
    /// would go nowhere while `mm ls` was plainly reaching it.
    #[tokio::test]
    async fn a_machine_a_client_reached_is_watched_again() {
        let node = watching_nothing().await;
        assert!(!node.peers.watched(NOWHERE));

        node.rewatch(&[NOWHERE.to_string()], &[NOWHERE.to_string()]);

        assert!(
            node.peers.watched(NOWHERE),
            "a watched machine that answered a client is subscribed to again"
        );
    }

    /// A client reaches machines nobody asked to be notified about: an ssh
    /// destination typed once is not `mm add`.
    #[tokio::test]
    async fn a_machine_that_is_not_watched_is_not_watched_now_either() {
        let node = watching_nothing().await;

        node.rewatch(&[NOWHERE.to_string()], &[]);

        assert!(!node.peers.watched(NOWHERE));
        assert!(node.peers.is_empty(), "and nothing was started for it");
    }

    /// Re-arming has to leave a live subscription alone, or every `mm ls` would
    /// drop the events feed on the floor and build a new one.
    #[tokio::test]
    async fn a_machine_already_watched_is_left_alone() {
        let node = watching_nothing().await;
        let watched = [NOWHERE.to_string()];
        node.rewatch(&watched, &watched);
        let first = node.peers.names();

        node.rewatch(&watched, &watched);

        assert_eq!(node.peers.names(), first);
        assert_eq!(node.peers.len(), 1, "one watcher, not two");
    }

    #[test]
    fn a_paste_is_only_written_once_it_is_whole() {
        let mut paste = Paste::default();
        paste.take(b"\x89PNG".to_vec());
        paste.take(b"\r\n\x1a\n".to_vec());
        assert_eq!(paste.data, b"\x89PNG\r\n\x1a\n");

        let path = paste.finish("png").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\x89PNG\r\n\x1a\n");
        assert!(path.extension().is_some_and(|kind| kind == "png"));
        std::fs::remove_file(path).unwrap();

        // And it is empty again, so the next paste is its own file.
        assert!(paste.finish("png").is_err(), "an empty paste is not a file");
    }

    #[test]
    fn a_paste_over_the_limit_is_refused_rather_than_truncated() {
        let mut paste = Paste::default();
        paste.take(vec![0u8; proto::MAX_PASTE]);
        paste.take(vec![0u8; 1]);
        assert!(paste.finish("png").is_err());
        // Nothing of it is left to turn up on the end of the next one.
        assert!(paste.data.is_empty());
        assert!(!paste.too_big);
    }
}
