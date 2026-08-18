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

use manymux::proto::Size;
use tokio::runtime::Runtime;

use crate::keys::{Identity, KnownHosts};
use crate::machine::{Connection, Machine};
use crate::screen::Frame;
use crate::session::{Session, State};
use crate::ssh::reach;

/// Anything that went wrong, in the one sentence worth showing somebody.
///
/// One variant, because the app does nothing different for different failures:
/// what it does is say what happened, and the difference that matters is the
/// wording. Where a decision does hang on it, as with a host key that changed,
/// it is a state rather than an error.
#[derive(Debug, uniffi::Error)]
pub enum Trouble {
    Reaching { why: String },
}

impl std::fmt::Display for Trouble {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self::Reaching { why } = self;
        write!(out, "{why}")
    }
}

impl std::error::Error for Trouble {}

impl From<anyhow::Error> for Trouble {
    fn from(error: anyhow::Error) -> Self {
        Self::Reaching {
            // The whole chain: `connecting to ...: connection refused` says
            // what to do about it and `connection refused` does not.
            why: format!("{error:#}"),
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
    runtime: Runtime,
    identity: Identity,
    known: KnownHosts,
}

#[uniffi::export]
impl Phone {
    /// Read this device's identity out of `dir`, generating one if there is
    /// none there yet.
    #[uniffi::constructor]
    pub fn kept_in(dir: String) -> Result<Arc<Self>, Trouble> {
        let dir = PathBuf::from(dir);
        let runtime = Runtime::new().map_err(|error| Trouble::Reaching {
            why: format!("starting the client: {error}"),
        })?;
        Ok(Arc::new(Self {
            runtime,
            identity: Identity::kept_at(&dir.join("id_ed25519"))?,
            known: KnownHosts::at(dir.join("known_hosts")),
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
            let connection = Connection::open(&machine, &self.identity, &self.known).await?;
            let reached = reach(&connection).await?;
            connection.close().await;
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

    /// Attach to one, and stay attached.
    ///
    /// Returns at once. Everything after it is [`Attach::state`] and
    /// [`Attach::take_frame`].
    pub fn attach(&self, machine: Machine, name: String, grid: Grid) -> Arc<Attach> {
        let _running = self.runtime.enter();
        Arc::new(Attach {
            session: Session::open(
                machine,
                self.identity.clone(),
                self.known.clone(),
                name,
                grid.into(),
            ),
        })
    }

    /// Forget a machine's host key, which is what somebody says after deciding
    /// that a key which changed is a machine they reinstalled.
    pub fn forget(&self, machine: Machine) -> Result<(), Trouble> {
        self.known.forget(&machine.at())?;
        Ok(())
    }
}

/// One attached session.
#[derive(uniffi::Object)]
pub struct Attach {
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
