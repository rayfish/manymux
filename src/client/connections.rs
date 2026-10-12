//! Recently used host transports, with no sessions attached while parked.
//!
//! Keep at most four, for two minutes each. The node's idle deadline is longer,
//! so an expired cached connection is discarded before it is normally closed
//! remotely. A connection can still break sooner; attach handles that by
//! opening a fresh stream. Nothing here preconnects or installs on a host.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::Connection;

const CAPACITY: usize = 4;
const RETAIN_FOR: Duration = Duration::from_secs(120);

struct Parked {
    connection: Connection,
    since: Instant,
}

#[derive(Default)]
pub struct Connections {
    hosts: HashMap<String, Parked>,
}

impl Connections {
    pub fn take(&mut self, host: &str) -> Option<Connection> {
        self.expire();
        self.hosts.remove(host).map(|parked| parked.connection)
    }

    pub fn insert(&mut self, host: String, connection: Connection) {
        self.expire();
        self.hosts.insert(
            host,
            Parked {
                connection,
                since: Instant::now(),
            },
        );
        if self.hosts.len() > CAPACITY {
            let oldest = self
                .hosts
                .iter()
                .min_by_key(|(_, parked)| parked.since)
                .map(|(host, _)| host.clone());
            if let Some(oldest) = oldest {
                self.hosts.remove(&oldest);
            }
        }
    }

    fn expire(&mut self) {
        self.hosts
            .retain(|_, parked| parked.since.elapsed() < RETAIN_FOR);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::Reader;
    use crate::proto::FrameReader;
    use tokio::io::{AsyncReadExt, DuplexStream, split};

    fn transport() -> (Connection, DuplexStream) {
        let (client, server) = tokio::io::duplex(64);
        let (read, write) = split(client);
        let read: Reader = FrameReader::new(Box::new(read));
        (
            Connection {
                ready: tokio::spawn(async move { Ok(read) }),
                write: Some(Box::new(write)),
                carrier: None,
            },
            server,
        )
    }

    #[tokio::test]
    async fn excess_and_expired_transports_are_closed() {
        let mut connections = Connections::default();
        let (connection, mut first) = transport();
        connections.insert("build-box".into(), connection);
        for host in ["host-a", "host-b", "host-c", "host-d"] {
            let (connection, _) = transport();
            connections.insert(host.into(), connection);
        }
        assert!(connections.take("build-box").is_none());
        assert_eq!(first.read(&mut [0]).await.unwrap(), 0);
        let (connection, mut expired) = transport();
        connections.insert("gpu-box".into(), connection);
        connections.hosts.get_mut("gpu-box").unwrap().since -= RETAIN_FOR;
        assert!(connections.take("gpu-box").is_none());
        assert_eq!(expired.read(&mut [0]).await.unwrap(), 0);
    }
}
