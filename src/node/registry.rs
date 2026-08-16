//! The set of live sessions on this host.

use std::collections::HashMap;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use anyhow::{Result, bail};
use tokio::sync::broadcast;
use tracing::info;

use super::session::{Session, default_name};
use crate::lock::held;
use crate::proto::{EventKind, SessionEvent, SessionInfo, SpawnSpec};

/// Events buffered per subscriber. A subscriber that falls this far behind is
/// missing bells anyway, so it is dropped rather than allowed to hold memory.
const EVENT_BACKLOG: usize = 256;

pub struct Registry {
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Everything happening in every session on this host, in one stream.
    events: broadcast::Sender<SessionEvent>,
    /// Clients attached anywhere on this host, counted once for the machine
    /// rather than per session: a bell reaches whoever is sitting here, whichever
    /// session they happen to be looking at.
    clients: Arc<AtomicUsize>,
}

impl Registry {
    pub fn new() -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BACKLOG);
        Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            events,
            clients: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Listen to everything happening on this host.
    pub fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.events.subscribe()
    }

    pub fn spawn(self: &Arc<Self>, spec: &SpawnSpec) -> Result<Arc<Session>> {
        let mut sessions = held(&self.sessions);
        prune(&mut sessions);

        let name = match &spec.name {
            Some(n) => vet(&sessions, n)?,
            None => unique_name(&sanitize(&default_name(spec)), |n| sessions.contains_key(n)),
        };

        let session = Session::spawn(
            spec,
            name.clone(),
            self.events.clone(),
            Arc::clone(&self.clients),
        )?;
        info!(session = %name, pid = session.pid, "spawned");
        sessions.insert(name, Arc::clone(&session));
        session.publish(EventKind::Started);

        // Drop the session once the child exits so `mm ls` reflects reality
        // and the PTY fd is released promptly. By what has exited rather than
        // by the name captured here, which a rename since would have moved:
        // removing that key takes another session's entry or none at all.
        let registry = Arc::clone(self);
        let exited = Arc::clone(&session);
        let mut exit_rx = session.exit_rx();
        tokio::spawn(async move {
            let _ = exit_rx.wait_for(|code| code.is_some()).await;
            prune(&mut held(&registry.sessions));
            info!(session = %exited.name(), "exited");
        });

        Ok(session)
    }

    pub fn get(&self, name: &str) -> Option<Arc<Session>> {
        let mut sessions = held(&self.sessions);
        prune(&mut sessions);
        sessions.get(name).cloned()
    }

    /// Every live session, for the one caller that acts on all of them at once
    /// rather than reporting them: the shutdown.
    pub fn all(&self) -> Vec<Arc<Session>> {
        let mut sessions = held(&self.sessions);
        prune(&mut sessions);
        sessions.values().map(Arc::clone).collect()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut sessions = held(&self.sessions);
        prune(&mut sessions);
        let mut out: Vec<_> = sessions.values().map(|s| s.info()).collect();
        // Oldest first, and by name only to break a tie: two sessions opened in
        // the same instant have to come out one way round every time, and a
        // host too old to stamp them at all ties every one of them, which is
        // how it keeps the name order it has always had.
        out.sort_by(|a, b| (a.started, &a.name).cmp(&(b.started, &b.name)));
        out
    }

    pub fn kill(&self, name: &str) -> Result<()> {
        let session = self.get(name).ok_or_else(|| no_such(name))?;
        session.kill();
        Ok(())
    }

    /// Move a session to a different name, and answer with the name it ended
    /// up with: what was asked for goes through the same sanitising a spawn's
    /// does, so the two are not always the same string.
    ///
    /// Refused rather than made unique when the name is another session's. A
    /// spawn has nobody to tell, so it takes the next free counter; a rename
    /// was typed at a prompt with somebody sitting in front of it, and quietly
    /// landing them on `build-2` is worse than saying the name is taken.
    pub fn rename(&self, name: &str, to: &str) -> Result<String> {
        let mut sessions = held(&self.sessions);
        prune(&mut sessions);
        if !sessions.contains_key(name) {
            return Err(no_such(name));
        }
        // Its own name is not a clash, so a rename to what it is called already
        // is a rename with nothing to do rather than an error.
        if sanitize(to) == name {
            return Ok(name.to_string());
        }
        let wanted = vet(&sessions, to)?;
        let session = sessions.remove(name).expect("checked just above");
        session.set_name(&wanted);
        sessions.insert(wanted.clone(), session);
        info!(from = %name, to = %wanted, "renamed");
        Ok(wanted)
    }
}

/// A name a caller asked for, ready to key a session by, or why it cannot be.
fn vet(sessions: &HashMap<String, Arc<Session>>, wanted: &str) -> Result<String> {
    let name = sanitize(wanted);
    if name.is_empty() {
        bail!("session name must contain at least one usable character");
    }
    if sessions.contains_key(&name) {
        bail!("session {name} already exists");
    }
    Ok(name)
}

fn no_such(name: &str) -> anyhow::Error {
    anyhow::anyhow!("no session named {name}")
}

/// Belt and braces alongside the per-session exit watcher: a session whose
/// child is gone must never be handed out for attach.
fn prune(sessions: &mut HashMap<String, Arc<Session>>) {
    sessions.retain(|_, s| !s.has_exited());
}

/// Session names appear in `host/name` paths and in the terminal, so keep them
/// to something unambiguous and safe to print.
///
/// A space becomes a dash rather than going the way of everything else that is
/// dropped: two words typed at the rename prompt are two words, and running
/// them together into `rayfishipv6` is nobody's idea of the name they asked
/// for.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect()
}

fn unique_name(base: &str, taken: impl Fn(&str) -> bool) -> String {
    let base = if base.is_empty() { "session" } else { base };
    if !taken(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range always yields a free name")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn sanitize_strips_path_and_control_characters() {
        assert_eq!(sanitize("../etc/passwd"), "..etcpasswd");
        assert_eq!(sanitize("my session\x07"), "my-session");
        assert_eq!(sanitize("build-1_a.b"), "build-1_a.b");
    }

    #[test]
    fn unique_name_appends_a_counter() {
        let mut taken: HashSet<String> = HashSet::new();
        assert_eq!(unique_name("zsh", |n| taken.contains(n)), "zsh");
        taken.insert("zsh".into());
        assert_eq!(unique_name("zsh", |n| taken.contains(n)), "zsh-2");
        taken.insert("zsh-2".into());
        assert_eq!(unique_name("zsh", |n| taken.contains(n)), "zsh-3");
    }

    #[test]
    fn unique_name_falls_back_when_the_base_is_empty() {
        assert_eq!(unique_name("", |_| false), "session");
    }
}
