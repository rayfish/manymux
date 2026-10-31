//! Private local connections and event subscriptions.
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;
#[cfg(unix)]
pub use unix::{peer_pid, serve};
#[cfg(windows)]
pub use windows::serve;

use crate::proto::{self, FrameReader, tag};
use anyhow::Result;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tracing::warn;

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
