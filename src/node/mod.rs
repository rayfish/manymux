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
pub mod notify;
pub mod paste;
pub mod peers;
pub mod registry;
pub mod session;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::client::Stream;
use crate::hosts::{Hosts, this_machine};
use crate::proto::{self, FrameReader, HostedEvent, Request, Response, tag};
use notify::Notifier;
use peers::Peers;
use registry::Registry;

/// How often to check whether the watched-hosts file changed.
const HOSTS_POLL: Duration = Duration::from_secs(2);

/// Events buffered per subscriber before it is considered too far behind.
const EVENT_BACKLOG: usize = 256;

/// How often an attached client is asked whether it is still there, and how
/// long it may go without answering before it is treated as gone.
///
/// Nothing underneath notices a client that vanished without detaching. A
/// closed laptop leaves sshd holding the session, sshd's own `ClientAlive`
/// probing is off by default, and the kernel's TCP keepalive is two hours away.
/// Until something says otherwise the phantom keeps its say in the session's
/// size and keeps counting as attached in `mm ls`.
const PING_EVERY: Duration = Duration::from_secs(15);
const SILENT_FOR: Duration = Duration::from_secs(45);

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
}

impl Node {
    pub async fn start(config: Config) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        let node = Arc::new(Self {
            registry: Registry::new(),
            peers: Peers::default(),
            notifier: Arc::new(Notifier::new(config.notifications)),
            events,
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
            let node = Arc::clone(self);
            let watching = host.clone();
            let task = tokio::spawn(async move { node.watch(watching).await });
            self.peers.watching(host, task);
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

    /// Subscribe to one machine's events for as long as it is watched.
    async fn watch(&self, host: String) {
        loop {
            if let Err(e) = self.subscribe_once(&host).await {
                debug!(host = %host, "event subscription ended: {e:#}");
            }
            tokio::time::sleep(peers::RESUBSCRIBE_DELAY).await;
        }
    }

    async fn subscribe_once(&self, host: &str) -> Result<()> {
        let mut stream = Stream::over_ssh(host).await?;
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

    /// Serve one stream: a single request, then either a response or, for
    /// `Attach` and `Events`, something lasting until the client goes away.
    pub async fn handle<R, W>(&self, read: R, mut write: W) -> Result<()>
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
                    env!("CARGO_PKG_VERSION")
                );
                return reply(&mut write, Err(complaint)).await;
            }
        };

        match request {
            Request::Attach { name, size } => {
                let Some(session) = self.registry.get(&name) else {
                    return reply(&mut write, Err(anyhow::anyhow!("no session named {name}")))
                        .await;
                };
                let attached = session.attach(size);
                let response = Response::Attached {
                    size: attached.size,
                    paste: true,
                };
                proto::write_msg(&mut write, tag::RESPONSE, &response).await?;
                pump_attachment(attached, read, write).await
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
                // The reply is in the socket buffer, not yet read. A moment for
                // the caller to take it costs nothing and saves a confusing
                // "connection reset" on the way out.
                tokio::spawn(async {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    std::process::exit(0);
                });
                Ok(())
            }

            request => reply(&mut write, self.answer(request)).await,
        }
    }

    /// Everything answered with a single response.
    fn answer(&self, request: Request) -> Result<Response> {
        match request {
            Request::List => Ok(Response::Sessions(self.registry.list())),
            Request::Spawn(spec) => self.registry.spawn(&spec).map(|session| Response::Spawned {
                name: session.name.clone(),
            }),
            Request::Kill { name } => self.registry.kill(&name).map(|()| Response::Ok),
            Request::Rename { name, title } => {
                self.registry.rename(&name, &title).map(|()| Response::Ok)
            }
            Request::Attach { .. } | Request::Events | Request::Stop => {
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
async fn pump_attachment<R, W>(
    attached: session::Attached,
    mut read: FrameReader<R>,
    mut write: W,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let session::Attached {
        attachment,
        repaint,
        mut output,
        size: _,
    } = attached;
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

    loop {
        tokio::select! {
            frame = read.next() => {
                let Some(frame) = frame? else { break };
                last_heard = tokio::time::Instant::now();
                match frame.tag {
                    tag::DATA => attachment.send_input(frame.body),
                    tag::RESIZE => attachment.resize(proto::decode(&frame.body)?),
                    tag::PONG => answers_pings = true,
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
pub async fn agent(socket: &Path) -> Result<()> {
    ensure_running(socket).await?;
    let mut node = tokio::net::UnixStream::connect(socket)
        .await
        .with_context(|| format!("connecting to {}", socket.display()))?;

    let mut stdio = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    tokio::io::copy_bidirectional(&mut stdio, &mut node)
        .await
        .context("relaying between ssh and the node")?;
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

/// Start a node in the background and wait for its socket to appear.
async fn start_node(socket: &Path) -> Result<()> {
    let binary = std::env::current_exe().context("finding the mm binary")?;
    info!("no node running, starting one");
    std::process::Command::new(&binary)
        .arg("--socket")
        .arg(socket)
        .arg("daemon")
        // Nothing is watching its output, and inheriting ssh's pipes would hold
        // the ssh session open for as long as the node lived.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
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

#[cfg(test)]
mod tests {
    use super::*;

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
