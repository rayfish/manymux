//! The boundary the app calls across.
//!
//! Kotlin never awaits Rust. It calls methods that either take the screen's
//! lock for as long as it takes to copy some rows, or drop a message into a
//! queue; everything that waits on a machine happens on tasks belonging to a
//! runtime this module owns. The two calls that genuinely have to wait, opening
//! a connection and asking what is running, say so by blocking, and the app
//! makes them off its main thread.
//!
//! The types crossing here are the ones the rest of the crate already has.
//! Nothing is redeclared for the boundary's sake, so there is no second
//! definition of a screen to drift from the first.

use std::path::PathBuf;
use std::sync::Arc;

use manymux::proto::{Size, SpawnSpec};
use tokio::runtime::Runtime;

use crate::keys::{Identity, KnownHosts};
use crate::machine::{Connection, Connections, Machine, Rebuff};
use crate::screen::Frame;
use crate::session::{Session, State};
use crate::ssh::{reach, start};

/// Anything that went wrong, in the one sentence worth showing somebody.
///
/// Three variants, and the split is by what the screen can *offer* rather than
/// by what happened: [`Trouble::Reaching`] has a sentence and a "try again",
/// while the other two each have one button that fixes them. Anything the app
/// would answer the same way stays in the first.
///
/// `flat_error` is load-bearing rather than a detail. Without it UniFFI builds
/// the Kotlin exception's message out of the variant's *fields*, so a phone
/// showed `why=gpu-box:22 would not take this device's key`: the name of a
/// field in this file, on a screen, in front of somebody trying to work out
/// what to do. With it the message is this type's `Display` and nothing else.
#[derive(Debug, uniffi::Error)]
#[uniffi(flat_error)]
pub enum Trouble {
    /// Something to read and a "try again" behind it.
    Reaching { why: String },
    /// The account has not been given this device's key. The app holds the
    /// key, so this is the failure it can hand somebody the fix for.
    Refused { why: String },
    /// The host key is not the one written down.
    HostKey { why: String },
}

impl Trouble {
    fn why(&self) -> &str {
        match self {
            Self::Reaching { why } | Self::Refused { why } | Self::HostKey { why } => why,
        }
    }
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{}", self.why())
    }
}

impl std::error::Error for Trouble {}

impl From<anyhow::Error> for Trouble {
    fn from(error: anyhow::Error) -> Self {
        // Downcast rather than read the sentence. A `Rebuff` is wrapped in
        // whatever context it passed through on the way up, and anyhow looks
        // through those, so the kind survives however deep it was raised.
        match error.downcast_ref::<Rebuff>() {
            // Its own sentence, not the chain: it names the machine in its
            // first four words, and `connecting to gpu-box:22: gpu-box:22
            // would not take` is a longer line that says less.
            Some(rebuff @ Rebuff::Key { .. }) => Self::Refused {
                why: rebuff.to_string(),
            },
            Some(rebuff @ Rebuff::Host { .. }) => Self::HostKey {
                why: rebuff.to_string(),
            },
            // The whole chain: `connecting to ...: connection refused` says
            // what to do about it and `connection refused` does not.
            None => Self::Reaching {
                why: format!("{error:#}"),
            },
        }
    }
}

/// The size of the phone's grid, in cells.
#[derive(Clone, Copy, uniffi::Record)]
pub struct Grid {
    pub cols: u16,
    pub rows: u16,
}

impl From<Grid> for Size {
    fn from(grid: Grid) -> Self {
        Size::new(grid.cols, grid.rows)
    }
}

/// One session on a machine, as a list shows it.
///
/// A flattening of `proto::SessionInfo`: the app draws four of these fields and
/// a timestamp it has no use for would still have to be given a representation.
#[derive(uniffi::Record)]
pub struct Running {
    pub name: String,
    pub title: String,
    pub command: String,
    /// How many clients are attached, which is what draws the dot.
    pub attached: u32,
    /// Seconds since anything was typed.
    pub idle: u64,
}

/// This device: its key, and what it knows about the machines it reaches.
///
/// One of these for the life of the app. It owns the runtime, so nothing else
/// has to know there is one.
#[derive(uniffi::Object)]
pub struct Phone {
    runtime: Arc<Runtime>,
    identity: Identity,
    known: KnownHosts,
    /// Shared by everything that reaches a machine, which is what makes going
    /// back to the list and into another session cost a round trip rather than
    /// a handshake.
    connections: Arc<Connections>,
}

#[uniffi::export]
impl Phone {
    /// Read this device's identity out of `dir`, generating one if there is
    /// none there yet.
    ///
    /// No agent, and not by omission. `SSH_AUTH_SOCK` is not a thing Android
    /// sets, and a socket belonging to another app is not one this process
    /// could open if it were: an agent is [`crate::agent`]'s answer for the
    /// desktop the `reach` example runs on. The key generated here is the
    /// phone's whole identity.
    #[uniffi::constructor]
    pub fn kept_in(dir: String) -> Result<Arc<Self>, Trouble> {
        let dir = PathBuf::from(dir);
        let runtime = Runtime::new().map_err(|error| Trouble::Reaching {
            why: format!("starting the client: {error}"),
        })?;
        Ok(Arc::new(Self {
            runtime: Arc::new(runtime),
            identity: Identity::kept_at(&dir.join("id_ed25519"))?,
            known: KnownHosts::at(dir.join("known_hosts")),
            connections: Arc::new(Connections::none()),
        }))
    }

    /// The line to paste into a machine's `authorized_keys`.
    pub fn authorized_line(&self) -> String {
        self.identity.authorized_line()
    }

    pub fn fingerprint(&self) -> String {
        self.identity.fingerprint()
    }

    /// What is running on a machine.
    ///
    /// Blocks: it is a connection, a ladder and a round trip. Called off the
    /// app's main thread.
    pub fn running_on(&self, machine: Machine) -> Result<Vec<Running>, Trouble> {
        self.runtime.block_on(async {
            let connection = self.reach_for(&machine).await?;
            let reached = match reach(&*connection).await {
                Ok(reached) => reached,
                Err(error) => {
                    // Held connections go stale silently on a phone, so a
                    // failure on one is a failure of the pool as much as of
                    // the request: dropped here, the retry somebody makes by
                    // pressing the button again opens a fresh one.
                    self.connections.forget().await;
                    return Err(error.into());
                }
            };
            Ok(reached
                .sessions
                .into_iter()
                .map(|session| Running {
                    name: session.name,
                    title: session.title,
                    command: session.command,
                    attached: session.attached as u32,
                    idle: session.idle,
                })
                .collect())
        })
    }

    /// Start a new session there and answer with what it ended up called.
    ///
    /// A login shell and nothing else. The node runs a spawn as
    /// `shell -lc <command>` with the words quoted one at a time, so a command
    /// line typed at a phone would have to be split the way a shell splits it
    /// before it could be sent, and getting that half right is a keyboard this
    /// version does not have.
    ///
    /// Blocks, like [`Phone::running_on`].
    pub fn start_on(&self, machine: Machine, grid: Grid) -> Result<String, Trouble> {
        self.runtime.block_on(async {
            let connection = self.reach_for(&machine).await?;
            let name = start(
                &*connection,
                SpawnSpec {
                    name: None,
                    command: Vec::new(),
                    cwd: None,
                    size: grid.into(),
                    label: None,
                },
            )
            .await?;
            Ok(name)
        })
    }

    /// Attach to one, and stay attached.
    ///
    /// Returns at once. Everything after it is [`Attach::state`] and
    /// [`Attach::take_frame`].
    pub fn attach(&self, machine: Machine, name: String, grid: Grid) -> Arc<Attach> {
        let _running = self.runtime.enter();
        Arc::new(Attach {
            // The runtime is held here as well as by the phone: an attach that
            // outlived the object that started it would have its tasks stop
            // without a word, or take a thread down with them.
            runtime: Arc::clone(&self.runtime),
            session: Session::open(
                machine,
                self.identity.clone(),
                self.known.clone(),
                Arc::clone(&self.connections),
                name,
                grid.into(),
            ),
        })
    }

    /// Forget a machine's host key, which is what somebody says after deciding
    /// that a key which changed is a machine they reinstalled.
    ///
    /// The connection goes with it: one held to the machine whose key is being
    /// argued about is one authenticated against the key that is in doubt.
    pub fn forget(&self, machine: Machine) -> Result<(), Trouble> {
        self.known.forget(&machine.at())?;
        self.runtime.block_on(self.connections.forget());
        Ok(())
    }
}

impl Phone {
    /// The connection to a machine, from the pool.
    ///
    /// Not exported: the app names a machine and this decides whether that
    /// costs a handshake, which is exactly the decision it should not have to
    /// make or even know about.
    async fn reach_for(&self, machine: &Machine) -> Result<Arc<Connection>, Trouble> {
        Ok(self
            .connections
            .to(machine, &self.identity, &self.known)
            .await?)
    }
}

/// One attached session.
#[derive(uniffi::Object)]
pub struct Attach {
    #[allow(dead_code)]
    runtime: Arc<Runtime>,
    session: Session,
}

#[uniffi::export]
impl Attach {
    /// The rows that have changed since the last frame.
    ///
    /// Called once a frame, and cheap enough to call when nothing has changed:
    /// a frame with no rows in it is a frame the widget skips.
    pub fn take_frame(&self) -> Frame {
        self.session.take_frame()
    }

    /// Keystrokes, or pasted text.
    pub fn send(&self, bytes: Vec<u8>) {
        self.session.send(bytes);
    }

    pub fn resize(&self, grid: Grid) {
        self.session.resize(grid.into());
    }

    /// Leave, without ending anything on the machine.
    pub fn detach(&self) {
        self.session.detach();
    }

    pub fn state(&self) -> State {
        self.session.state()
    }
}

#[cfg(test)]
mod tests {
    use anyhow::anyhow;

    use super::Trouble;
    use crate::machine::Rebuff;

    /// The two failures the app can offer something about arrive typed, and
    /// they arrive typed from underneath whatever context was wrapped around
    /// them on the way up. Read off the sentence instead, as this once was, a
    /// reworded message silently turns the button that fixes it into no button
    /// at all.
    #[test]
    fn a_refusal_worth_acting_on_keeps_its_kind_across_the_boundary() {
        let refused = anyhow!(Rebuff::Key {
            at: "gpu-box:22".to_string(),
            user: "somebody".to_string(),
            fingerprint: "SHA256:whatever".to_string(),
        })
        .context("connecting to gpu-box:22");
        assert!(matches!(Trouble::from(refused), Trouble::Refused { .. }));

        let changed = anyhow!(Rebuff::Host {
            at: "gpu-box:22".to_string(),
            had: "SHA256:before".to_string(),
            now: "SHA256:after".to_string(),
        })
        .context("connecting to gpu-box:22");
        assert!(matches!(Trouble::from(changed), Trouble::HostKey { .. }));
    }

    /// And what it says is the refusal's own sentence, not the chain around
    /// it. The context is there for the failures nobody typed a message for,
    /// where `connection refused` alone does not say what to do; a rebuff
    /// already names the machine in its first four words, and prefixing it
    /// with the same machine again is a longer message that says less.
    #[test]
    fn a_refusal_is_shown_as_the_sentence_it_wrote_for_the_screen() {
        let refused = anyhow!(Rebuff::Key {
            at: "gpu-box:22".to_string(),
            user: "somebody".to_string(),
            fingerprint: "SHA256:whatever".to_string(),
        })
        .context("connecting to gpu-box:22");
        let said = Trouble::from(refused).to_string();
        assert!(!said.contains("connecting to"), "{said}");
        assert!(said.contains("SHA256:whatever"), "{said}");
    }

    /// Everything else keeps the whole chain, which is the reason the chain is
    /// formatted at all: `connection refused` says nothing about which machine
    /// or which port, and that is the whole of what somebody needs.
    #[test]
    fn anything_else_is_still_shown_with_what_it_was_doing() {
        let ordinary = anyhow!("connection refused").context("connecting to gpu-box:22");
        let said = Trouble::from(ordinary).to_string();
        assert!(said.contains("connecting to gpu-box:22"), "{said}");
        assert!(said.contains("connection refused"), "{said}");
    }
}
