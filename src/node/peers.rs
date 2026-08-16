//! Watching the other machines you care about.
//!
//! There are no connections to manage here. ssh does that: every request runs
//! `ssh <host> mm agent`, and a shared connection (`ControlMaster`) makes
//! the second one to a host cheap. What is left is keeping one long-lived
//! subscription per machine, so a bell in a session nobody is watching still
//! reaches the desktop you are sitting at.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::task::JoinHandle;
use tracing::info;

use crate::lock::held;

/// How long to wait before re-subscribing to a machine's events after the
/// subscription ends. Guards against a tight loop when a host is down, or
/// answers and hangs up.
pub const RESUBSCRIBE_DELAY: Duration = Duration::from_secs(5);

/// The machines being watched, and the task watching each one.
#[derive(Default)]
pub struct Peers {
    watchers: Mutex<HashMap<String, JoinHandle<()>>>,
}

impl Peers {
    /// Make the watched set match `wanted`, returning the ones that are new so
    /// the caller can start watching them.
    ///
    /// Machines already watched are left alone: pairing with one must not
    /// interrupt the subscription to another.
    pub fn sync(&self, wanted: &[String]) -> Vec<String> {
        let mut watchers = held(&self.watchers);

        let gone: Vec<String> = watchers
            .keys()
            .filter(|host| !wanted.contains(host))
            .cloned()
            .collect();
        for host in gone {
            if let Some(watcher) = watchers.remove(&host) {
                watcher.abort();
                info!(host = %host, "no longer watching");
            }
        }

        wanted
            .iter()
            .filter(|host| !watchers.contains_key(*host))
            .cloned()
            .collect()
    }

    /// Record the task watching `host`, so dropping the host stops it.
    pub fn watching(&self, host: String, task: JoinHandle<()>) {
        held(&self.watchers).insert(host, task);
    }

    pub fn names(&self) -> Vec<String> {
        let mut names: Vec<String> = held(&self.watchers).keys().cloned().collect();
        names.sort();
        names
    }

    pub fn len(&self) -> usize {
        held(&self.watchers).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Shared so the watcher tasks can outlive the call that started them.
pub type Shared = Arc<Peers>;

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> JoinHandle<()> {
        tokio::spawn(std::future::pending())
    }

    #[tokio::test]
    async fn syncing_only_reports_what_is_new() {
        let peers = Peers::default();

        let added = peers.sync(&["one".to_string(), "two".to_string()]);
        assert_eq!(added, vec!["one", "two"]);
        for host in added {
            peers.watching(host, idle());
        }

        // Drop `two`, keep `one`, add `three`.
        let added = peers.sync(&["one".to_string(), "three".to_string()]);
        assert_eq!(added, vec!["three"], "an unchanged host is not restarted");
        assert_eq!(peers.names(), vec!["one"], "the dropped host is gone");
    }

    #[tokio::test]
    async fn dropping_a_host_stops_watching_it() {
        let peers = Peers::default();
        peers.sync(&["one".to_string()]);
        let task = idle();
        let handle = task.abort_handle();
        peers.watching("one".to_string(), task);

        peers.sync(&[]);
        assert!(peers.is_empty());
        // Give the runtime a moment to actually cancel it.
        tokio::task::yield_now().await;
        assert!(handle.is_finished(), "the watcher should have been aborted");
    }
}
