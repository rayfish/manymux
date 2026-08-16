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

/// What to wait before trying a machine again, by how many tries have failed
/// since it was last reached. Running out is the end of it: the watcher returns
/// and nothing on a timer starts it again.
///
/// A machine that is off the network stays off it for hours, not seconds, and
/// the retries cost more than they look like they do. Each one is an `ssh`
/// process, a name to resolve and a connect to wait out, per host, on a laptop
/// that is usually asleep or on a different network. Four added machines at a
/// fixed five seconds is close to one process a second, all night. So the
/// delays grow and then stop, and coming back is left to the one thing that
/// actually knows the network is different: somebody typing `mm ls`.
const RETRIES: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(45),
];

/// How long a subscription has to last to count as the machine having been
/// reached, rather than a machine that takes the connection and drops it.
/// Under this, the attempts keep counting up and the watcher still gives up.
pub const STEADY: Duration = Duration::from_secs(60);

/// How long to wait before trying `attempt` again, or `None` to stop trying.
pub fn retry_after(attempt: u32) -> Option<Duration> {
    RETRIES.get(attempt as usize - 1).copied()
}

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
            .filter(|host| {
                watchers
                    .get(*host)
                    .is_none_or(|watcher| watcher.is_finished())
            })
            .cloned()
            .collect()
    }

    /// Whether a watcher for `host` is still running.
    ///
    /// A watcher that gave up is a finished task still sitting in the map, and
    /// that is deliberate: the task cannot tidy its own entry away without
    /// racing whoever is replacing it, while a handle can always be asked
    /// whether it is over. Finished means not watched, everywhere.
    pub fn watched(&self, host: &str) -> bool {
        held(&self.watchers)
            .get(host)
            .is_some_and(|watcher| !watcher.is_finished())
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

    /// A host that is off the network is not worth an ssh every five seconds
    /// for as long as it stays that way: a laptop with four machines added and
    /// no connection would spawn one a second between them, for hours.
    #[test]
    fn a_host_that_never_answers_is_given_up_on() {
        assert_eq!(retry_after(1), Some(Duration::from_secs(5)));
        assert_eq!(retry_after(2), Some(Duration::from_secs(15)));
        assert_eq!(retry_after(3), Some(Duration::from_secs(45)));
        assert_eq!(retry_after(4), None, "and then it stops asking");
        assert_eq!(retry_after(50), None);
    }

    /// Giving up has to leave the host lookable-at again, or `mm ls` reaching
    /// it would have no way to start the subscription back up.
    #[tokio::test]
    async fn a_host_given_up_on_is_no_longer_watched() {
        let peers = Peers::default();
        peers.sync(&["one".to_string()]);
        peers.watching("one".to_string(), idle());
        assert!(peers.watched("one"));

        // The watcher ran out of attempts and returned.
        let over = tokio::spawn(std::future::ready(()));
        peers.watching("one".to_string(), over);
        tokio::task::yield_now().await;

        assert!(!peers.watched("one"), "a finished watcher is not a watcher");
        assert_eq!(
            peers.sync(&["one".to_string()]),
            vec!["one"],
            "so anything resyncing picks it back up"
        );
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
