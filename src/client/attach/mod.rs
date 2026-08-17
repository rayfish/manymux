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

use std::time::Duration;

use anyhow::Result;

use crate::client::{Attached, SessionHalves, Update};

mod keys;
#[cfg(feature = "desktop")]
mod terminal;

pub use keys::{
    Action, DEFAULT_PREFIX, Find, KeyFilter, Keystrokes, Mode, Motion, PASTE_KEY, Pick, Rename,
    Scroll, Select, Spot, prefix,
};

use super::picker;

#[cfg(feature = "desktop")]
pub use terminal::{Held, PopupFeed, Wait, hold, run, session_size, terminal_size, waiting};

/// How long to wait before trying to reach a lost session again, by how many
/// tries have failed.
///
/// Short at first because the common reason is a wifi hop or a laptop lid,
/// which is over in seconds, and then flat rather than doubling: the terminal
/// is sitting there showing the session, and the difference between checking
/// every ten seconds and every minute is whether it comes back by itself or
/// you go and do it yourself.
///
/// It never stops. The session is still running on a machine that never
/// noticed the client left, so there is nothing here for a clock to decide:
/// coming back to a laptop after a weekend and finding the session still on
/// the screen is what the project is for, and the way out is the keystroke
/// [`Wait`] is waiting for rather than an attempt somebody ran out of.
pub fn reconnect_after(attempt: u32) -> Duration {
    Duration::from_secs(2u64.saturating_pow(attempt.saturating_sub(1)).min(SLOWEST))
}

/// The delay every attempt past the first few gets. Slow enough that a machine
/// which is off for days is not costing an ssh process, a name to resolve and
/// a connect to wait out every second, quick enough that a lid opened again is
/// back before a hand reaches the keyboard.
const SLOWEST: u64 = 10;

/// How the attach ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Detached,
    /// A popup was closed with something chosen. What each row means is the
    /// caller's: it handed the rows in and gets their ids back, so this half of
    /// the client never learns what a host, a group or a pid is.
    Chose(Chose),
    /// A switch key was pressed. Which session it lands on is the caller's to
    /// work out: this half of the client knows nothing about hosts.
    Switch(Motion),
    /// The new key was pressed. Starting the session and landing in it is the
    /// caller's for the same reason.
    New,
    /// The session's process exited with this code.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

/// What a popup was closed with, in the caller's own row ids.
///
/// Every one of these needs something this half of the client is kept from
/// knowing: assigning needs a config file and a session's pid, renaming a
/// session that is not the one attached needs a connection to its machine, and
/// narrowing needs the listing. So they are handed back the way a switch is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Chose {
    /// Attach to the session on this row.
    Go(usize),
    /// Move the session on `session` into the group on `group`.
    Move { session: usize, group: usize },
    /// Narrow to the group on this row.
    Focus(usize),
    /// Rename the session on `session`.
    Named { session: usize, to: String },
    /// Make a group and put the session on `session` in it. One act rather than
    /// two, because a group is a set of live sessions and an empty one cannot
    /// exist.
    NewGroup { session: usize, name: String },
}

/// The rows a popup is drawn from, handed in per attach.
///
/// Both lists at once because `m` swaps from one to the other without a round
/// trip, and asking for the groups at that moment would put an ssh in the
/// middle of a keystroke.
#[derive(Debug, Default, Clone)]
pub struct Rows {
    pub sessions: Vec<picker::Row>,
    pub groups: Vec<picker::Row>,
    /// Which session row is the one being attached to, so the popup opens with
    /// the highlight already on it.
    pub at: usize,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped connection is usually a wifi hop or a laptop lid, so the
    /// first tries are quick. What it must not do is give up on the first
    /// failure, or keep spending an ssh process every second on a machine
    /// that is plainly not there.
    #[test]
    fn a_reconnect_tries_quickly_at_first_and_then_settles() {
        assert_eq!(reconnect_after(1), Duration::from_secs(1));
        assert_eq!(reconnect_after(2), Duration::from_secs(2));
        assert_eq!(reconnect_after(3), Duration::from_secs(4));
        assert_eq!(reconnect_after(4), Duration::from_secs(8));
        assert_eq!(
            reconnect_after(5),
            Duration::from_secs(SLOWEST),
            "and flat from there"
        );
    }

    /// The session is still running on a machine that never noticed anybody
    /// left, so there is nothing here worth giving up on. Somebody walking
    /// away for a weekend and coming back to their session is the whole point
    /// of the project, and the way out is a keystroke rather than a clock.
    #[test]
    fn a_reconnect_never_gives_up_by_itself() {
        for attempt in [20, 500, 100_000, u32::MAX] {
            assert_eq!(
                reconnect_after(attempt),
                Duration::from_secs(SLOWEST),
                "attempt {attempt}"
            );
        }
    }

    /// There is no attempt zero: the count is incremented before the wait it
    /// pays for. Answered rather than left to overflow, since a delay of none
    /// would be a loop spinning ssh as fast as it can start it.
    #[test]
    fn a_reconnect_that_has_not_failed_yet_still_waits() {
        assert_eq!(reconnect_after(0), Duration::from_secs(1));
    }
}
