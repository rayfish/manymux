//! Which session a switch key lands on.
//!
//! [`super::attach`] reads the key and knows nothing else: it has no socket, no
//! host list, and no way to ask a node what is running. So it reports the
//! [`Motion`] and this works out where that goes, from a listing the caller
//! keeps up to date.

use crate::client::attach::Motion;

/// A session addressed absolutely: which machine, and which name on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub host: String,
    pub session: String,
}

impl Located {
    pub fn new(host: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            host: host.into(),
            session: session.into(),
        }
    }
}

/// The sessions a switch key can reach, and where in them you are.
///
/// The listing is a snapshot the caller refreshes; it is deliberately allowed
/// to be stale, because a keystroke must not wait on a machine that is asleep.
pub struct Cycle {
    /// Every session on every watched machine, in listing order.
    sessions: Vec<Located>,
    current: Located,
    /// Where the last hop came from, for [`Motion::Last`].
    previous: Option<Located>,
}

impl Cycle {
    pub fn new(current: Located) -> Self {
        Self {
            sessions: Vec::new(),
            current,
            previous: None,
        }
    }

    pub fn current(&self) -> &Located {
        &self.current
    }

    /// Take a new listing, ordered the way `mm ls` orders one so that walking
    /// the cycle matches what you would have read off the table.
    pub fn refresh(&mut self, mut sessions: Vec<Located>) {
        sessions.sort_by(|a, b| (&a.host, &a.session).cmp(&(&b.host, &b.session)));
        self.sessions = sessions;
    }

    /// Where a motion lands, or `None` when it lands where you already are.
    pub fn step(&self, motion: Motion) -> Option<Located> {
        let next = match motion {
            Motion::Last => self.previous.clone()?,
            Motion::Next | Motion::Previous => {
                let len = self.sessions.len();
                if len == 0 {
                    return None;
                }
                // A session that has since exited leaves us outside the list, so
                // start from whichever end the motion is heading towards.
                let at = self.sessions.iter().position(|s| *s == self.current);
                let index = match (motion, at) {
                    (Motion::Next, Some(i)) => (i + 1) % len,
                    (Motion::Next, None) => 0,
                    (Motion::Previous, Some(i)) => (i + len - 1) % len,
                    (Motion::Previous, None) => len - 1,
                    (Motion::Last, _) => unreachable!("handled above"),
                };
                self.sessions[index].clone()
            }
        };
        (next != self.current).then_some(next)
    }

    /// Record a hop, so `Motion::Last` knows where to come back to.
    pub fn moved_to(&mut self, next: Located) {
        self.previous = Some(std::mem::replace(&mut self.current, next));
    }

    /// Take back the last hop, leaving no trail: for one that could not be
    /// made, where coming back must not become the place `Motion::Last` goes.
    pub fn undo(&mut self) {
        if let Some(previous) = self.previous.take() {
            self.current = previous;
        }
    }

    /// Drop a session from the listing, for one that turned out to be gone.
    pub fn forget(&mut self, gone: &Located) {
        self.sessions.retain(|session| session != gone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(names: &[&str], current: &str) -> Cycle {
        let mut cycle = Cycle::new(Located::new("here", current));
        cycle.refresh(names.iter().map(|n| Located::new("here", *n)).collect());
        cycle
    }

    fn step(cycle: &Cycle, motion: Motion) -> Option<String> {
        cycle.step(motion).map(|next| next.session)
    }

    #[test]
    fn next_and_previous_wrap_around_the_ends() {
        let at_the_end = cycle(&["api", "build", "web"], "web");
        assert_eq!(step(&at_the_end, Motion::Next).as_deref(), Some("api"));
        let at_the_start = cycle(&["api", "build", "web"], "api");
        assert_eq!(
            step(&at_the_start, Motion::Previous).as_deref(),
            Some("web")
        );
    }

    #[test]
    fn the_listing_is_walked_in_the_order_mm_ls_shows_it() {
        let mut cycle = Cycle::new(Located::new("here", "api"));
        cycle.refresh(vec![
            Located::new("here", "web"),
            Located::new("gpu-box", "train"),
            Located::new("here", "api"),
        ]);
        // Sorted by machine, then name: gpu-box/train, here/api, here/web.
        assert_eq!(step(&cycle, Motion::Next).as_deref(), Some("web"));
        assert_eq!(step(&cycle, Motion::Previous).as_deref(), Some("train"));
    }

    #[test]
    fn the_only_session_there_is_has_nowhere_to_go() {
        let cycle = cycle(&["api"], "api");
        assert_eq!(step(&cycle, Motion::Next), None);
        assert_eq!(step(&cycle, Motion::Previous), None);
    }

    #[test]
    fn a_listing_that_has_not_landed_yet_lands_nowhere() {
        let cycle = Cycle::new(Located::new("here", "api"));
        assert_eq!(step(&cycle, Motion::Next), None);
    }

    #[test]
    fn a_session_that_has_since_exited_starts_from_the_end_you_are_heading_for() {
        let cycle = cycle(&["api", "build"], "gone");
        assert_eq!(step(&cycle, Motion::Next).as_deref(), Some("api"));
        assert_eq!(step(&cycle, Motion::Previous).as_deref(), Some("build"));
    }

    #[test]
    fn a_hop_that_could_not_be_made_leaves_no_trail_and_no_entry() {
        let mut cycle = cycle(&["api", "build", "web"], "api");
        let gone = Located::new("here", "build");
        cycle.moved_to(gone.clone());
        cycle.forget(&gone);
        cycle.undo();
        assert_eq!(cycle.current().session, "api");
        // Neither the way back nor the way on goes to the session that is gone.
        assert_eq!(step(&cycle, Motion::Last), None);
        assert_eq!(step(&cycle, Motion::Next).as_deref(), Some("web"));
    }

    #[test]
    fn last_bounces_between_the_two_you_have_been_on() {
        let mut cycle = cycle(&["api", "build", "web"], "api");
        // Nowhere to come back to until a hop has happened.
        assert_eq!(step(&cycle, Motion::Last), None);
        cycle.moved_to(Located::new("here", "web"));
        assert_eq!(step(&cycle, Motion::Last).as_deref(), Some("api"));
        cycle.moved_to(Located::new("here", "api"));
        assert_eq!(step(&cycle, Motion::Last).as_deref(), Some("web"));
    }
}
