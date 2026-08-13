//! The local Unix socket both long-running halves listen on.
//!
//! Whoever can reach one of these sockets can run commands as this user, so
//! they are owner-only and there is no further authentication behind them.
//! Remote access is a different thing entirely, authorized by iroh identity.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixListener;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::proto::{self, tag};

/// Accept connections on `socket` forever, handling each in its own task.
pub async fn serve<F, Fut>(socket: &Path, handle: F) -> Result<()>
where
    F: Fn(OwnedReadHalf, OwnedWriteHalf) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let listener = listen(socket).await?;
    let handle = Arc::new(handle);
    loop {
        let (stream, _) = listener.accept().await?;
        let handle = Arc::clone(&handle);
        tokio::spawn(async move {
            let (read, write) = stream.into_split();
            if let Err(e) = handle(read, write).await {
                debug!("connection ended: {e:#}");
            }
        });
    }
}

/// The longest a Unix socket path may be: 104 bytes on macOS, 108 on Linux, so
/// use the smaller and leave room for the NUL. Worth checking by hand, because
/// the error the kernel gives instead is unhelpfully generic.
const MAX_SOCKET_PATH: usize = 103;

/// Bind the socket, clearing one left behind by a crash but never one that is
/// still in use.
async fn listen(socket: &Path) -> Result<UnixListener> {
    if socket.as_os_str().len() > MAX_SOCKET_PATH {
        bail!(
            "{} is too long for a socket path ({} bytes, limit {MAX_SOCKET_PATH})",
            socket.display(),
            socket.as_os_str().len()
        );
    }
    if let Some(dir) = socket.parent() {
        crate::config::ensure_private_dir(dir)?;
    }
    if socket.exists() && tokio::net::UnixStream::connect(socket).await.is_ok() {
        bail!("something is already listening on {}", socket.display());
    }
    std::fs::remove_file(socket).ok();

    let listener =
        UnixListener::bind(socket).with_context(|| format!("binding {}", socket.display()))?;
    set_mode(socket)?;
    info!(socket = %socket.display(), "listening");
    Ok(listener)
}

fn set_mode(socket: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", socket.display()))
}

/// Feed a subscriber every event until it goes away.
///
/// The read half is watched only to notice the client leaving: a subscriber
/// sends nothing, so anything arriving on it is the end of the stream.
pub async fn pump_events<T, R, W>(
    mut events: broadcast::Receiver<T>,
    mut read: R,
    mut write: W,
) -> Result<()>
where
    T: Serialize + Clone,
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => proto::write_msg(&mut write, tag::EVENT, &event).await?,
                // A subscriber too slow to keep up has missed events. Say so and
                // carry on rather than dropping it: the next bell still matters.
                Err(RecvError::Lagged(n)) => warn!("an event subscriber missed {n} events"),
                Err(RecvError::Closed) => return Ok(()),
            },
            frame = proto::read_frame(&mut read) => {
                let _ = frame?;
                return Ok(());
            }
        }
    }
}
