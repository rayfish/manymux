//! Account-only named pipes. Keep a spare instance open while accepting so
//! another process cannot take the name between client connections.
use anyhow::Result;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{ReadHalf, WriteHalf, split};
use tokio::net::windows::named_pipe::NamedPipeServer;
use tokio::task::JoinSet;
use tracing::debug;

pub async fn serve<F, Fut>(socket: &Path, handle: F) -> Result<()>
where
    F: Fn(ReadHalf<NamedPipeServer>, WriteHalf<NamedPipeServer>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let mut server = crate::windows::pipe_server(socket, true)?;
    let handle = Arc::new(handle);
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            connected = server.connect(), if clients.len() < 256 => {
                connected?;
                let next = crate::windows::pipe_server(socket, false)?;
                let connected = std::mem::replace(&mut server, next);
                let handle = Arc::clone(&handle);
                clients.spawn(async move {
                    let (read, write) = split(connected);
                    handle(read, write).await
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                match result {
                    Ok(Ok(())) => {},
                    Ok(Err(error)) => debug!("connection ended: {error:#}"),
                    Err(error) => debug!("connection task failed: {error}"),
                }
            }
        }
    }
}
