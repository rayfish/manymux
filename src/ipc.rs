//! The local Unix socket both long-running halves listen on.
//!
//! Whoever can reach one of these sockets can run commands as this user, so
//! they are owner-only and there is no further authentication behind them.
//! Reaching one from another machine means going through sshd first, which is
//! where access is decided.

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

use crate::proto::{self, FrameReader, tag};

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

/// The pid of the process on the other end of a connection.
///
/// The way to reach a node that will not answer: it is whatever process is
/// holding this socket, and the kernel already knows which one that is. Only
/// Linux and macOS are covered, which is what manymux publishes builds for.
pub fn peer_pid(stream: &tokio::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;

    let fd = stream.as_raw_fd();
    #[cfg(target_os = "linux")]
    {
        let mut cred = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut len = size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the fd is open for the length of this call, and the buffer
        // matches the size handed to the kernel.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        (got == 0 && cred.pid > 0).then_some(cred.pid as u32)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut len = size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: as above.
        let got = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        (got == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        None
    }
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
    mut read: FrameReader<R>,
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
            frame = read.next() => {
                let _ = frame?;
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fallback in `mm update` leans on this: a node that will not answer
    /// is still identifiable by the socket it holds.
    #[tokio::test]
    async fn a_connection_names_the_process_on_the_other_end() {
        let dir = std::env::temp_dir().join(format!("manymux-peer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("t.sock");
        let listener = UnixListener::bind(&socket).unwrap();

        let client = tokio::net::UnixStream::connect(&socket).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();

        // Both ends are this test process, so both readings have to name it.
        assert_eq!(peer_pid(&client), Some(std::process::id()));
        assert_eq!(peer_pid(&server), Some(std::process::id()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
