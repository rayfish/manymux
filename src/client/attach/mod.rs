//! Driving a real terminal from an attached session.
//!
//! The terminal-specific half of the client. A mobile app skips this entirely
//! and drives [`crate::client::Attached`] directly, feeding the bytes to its
//! own terminal widget.
//!
//! The client stays deliberately dumb: raw mode, forward keystrokes, paint what
//! arrives, watch for the detach key. All the state lives on the server, which
//! is what makes detaching free.
//!
//! Whose screen it paints on is [`crate::client::screen`]'s to say, and the two
//! answers are different enough to be worth reading before this file.
//!
//! Split in three because only one of them needs a terminal. [`keys`] turns
//! bytes off stdin into what the client was asked for and is pure enough to
//! unit test whole; [`terminal`] is raw mode, escape sequences and the pump
//! loop, and is desktop-only; what is left here is the vocabulary they share
//! and the way in for a caller with no terminal at all.

use anyhow::Result;

use crate::client::{Attached, SessionHalves, Update};

mod keys;
#[cfg(feature = "desktop")]
mod terminal;

pub use keys::{
    Action, DEFAULT_PREFIX, Find, KeyFilter, Keystrokes, Mode, Motion, PASTE_KEY, Rename, Scroll,
    prefix,
};

#[cfg(feature = "desktop")]
pub use terminal::{Held, hold, run, session_size, terminal_size};

/// How the attach ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Detached,
    /// A switch key was pressed. Which session it lands on is the caller's to
    /// work out: this half of the client knows nothing about hosts.
    Switch(Motion),
    /// The session's process exited with this code.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

/// Everything an attached session produced before it stopped.
#[derive(Debug)]
pub struct Collected {
    pub output: Vec<u8>,
    pub outcome: Outcome,
}

/// Drive an attached session without a terminal, for tests and for clients that
/// render the output themselves.
pub async fn collect_until(
    session: Attached,
    mut stop: impl FnMut(&[u8]) -> bool,
) -> Result<Collected> {
    let SessionHalves { mut reader, .. } = session.split();
    let mut output = Vec::new();
    loop {
        let outcome = match reader.next().await? {
            // History counts as output here: a caller collecting a session's
            // bytes asked for the lines it asked for.
            Update::Output(bytes) | Update::Screen(bytes) | Update::History(bytes) => {
                output.extend_from_slice(&bytes);
                if stop(&output) {
                    Outcome::Detached
                } else {
                    continue;
                }
            }
            // Nothing here to answer with, the writing half having been
            // dropped. Never answering is what leaves the host holding this
            // client to no deadline, which is what a caller collecting output
            // without a connection to keep alive wants.
            Update::Ping => continue,
            // For a caller with no terminal to raise one on. A client that
            // renders sessions itself reads these from its own subscription.
            Update::Event(_) => continue,
            // Nothing here scrolls, so nothing here asked for a window or went
            // looking through one, and nothing here renames.
            Update::View(_) | Update::Found(_) | Update::Renamed(_) => continue,
            Update::Exited(code) => Outcome::Exited(code),
            Update::Disconnected => Outcome::Disconnected,
        };
        return Ok(Collected { output, outcome });
    }
}
