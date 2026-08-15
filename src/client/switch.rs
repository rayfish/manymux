//! Which session a switch key lands on.
//!
//! [`super::attach`] reads the key and knows nothing else: it has no socket, no
//! host list, and no way to ask a node what is running. So it reports the
//! [`Motion`] and this works out where that goes, from a listing the caller
//! keeps up to date.
//!
//! Tab stays on the machine you are on. Walking every session everywhere in one
//! run meant tabbing off the end of the machine you were working on landed you
//! on another one without asking, and left no way to say "the next machine"
//! outright. Machines are their own motion now.

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

/// Where a step lands in a list of `len`, given where you are in it. `None`
/// when the list is empty and there is nowhere to land at all.
///
/// Not being in the list is not the same as being at its start: a session that
/// has since exited, or a machine that has dropped out of the listing, leaves us
/// outside it, so the step starts from whichever end it is heading towards.
fn wrapped(forwards: bool, at: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    Some(match (forwards, at) {
        (true, Some(i)) => (i + 1) % len,
        (true, None) => 0,
        (false, Some(i)) => (i + len - 1) % len,
        (false, None) => len - 1,
    })
}

/// The sessions a switch key can reach, and where in them you are.
///
/// Two levels, because that is how the sessions are arranged: the sessions on
/// one machine, and the machines. `Motion::Next` walks the first and
/// `Motion::NextHost` the second.
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
    /// the cycle matches what you would have read off the table. Sorting by
    /// machine first is also what makes each machine's sessions a run, which is
    /// how the host motions find the first session on one.
    pub fn refresh(&mut self, mut sessions: Vec<Located>) {
        sessions.sort_by(|a, b| (&a.host, &a.session).cmp(&(&b.host, &b.session)));
        self.sessions = sessions;
    }

    /// Where a motion lands, or `None` when it lands where you already are.
    pub fn step(&self, motion: Motion) -> Option<Located> {
        let next = match motion {
            Motion::Last => self.previous.clone()?,
            Motion::Next | Motion::Previous => {
                // Only this machine's sessions: tab is for moving inside the
                // work you are already in, and `h` is for leaving it.
                let here: Vec<&Located> = self
                    .sessions
                    .iter()
                    .filter(|s| s.host == self.current.host)
                    .collect();
                let at = here.iter().position(|s| **s == self.current);
                let index = wrapped(motion == Motion::Next, at, here.len())?;
                here[index].clone()
            }
            Motion::NextHost | Motion::PreviousHost => {
                // The machines in listing order, which is the order the runs
                // above appear in.
                let mut hosts: Vec<&str> = Vec::new();
                for session in &self.sessions {
                    if hosts.last() != Some(&session.host.as_str()) {
                        hosts.push(&session.host);
                    }
                }
                let at = hosts.iter().position(|h| *h == self.current.host);
                let index = wrapped(motion == Motion::NextHost, at, hosts.len())?;
                let host = hosts[index];
                // The one machine there is is not a machine to go to. Landing
                // back on it would move you to a session you did not ask for.
                if host == self.current.host {
                    return None;
                }
                self.sessions.iter().find(|s| s.host == host)?.clone()
            }
        };
        (next != self.current).then_some(next)
    }

    /// Record a hop, so `Motion::Last` knows where to come back to.
    pub fn moved_to(&mut self, next: Located) {
        self.previous = Some(std::mem::replace(&mut self.current, next));
    }

    /// The session you are in is called something else now. Not a hop, so
    /// `Motion::Last` is left pointing where it was: nobody moved.
    pub fn renamed(&mut self, name: &str) {
        self.current.session = name.to_string();
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

    /// A cycle over several machines, currently on `host/session`.
    fn spread(sessions: &[(&str, &str)], current: (&str, &str)) -> Cycle {
        let mut cycle = Cycle::new(Located::new(current.0, current.1));
        cycle.refresh(
            sessions
                .iter()
                .map(|(host, name)| Located::new(*host, *name))
                .collect(),
        );
        cycle
    }

    /// Where a motion lands, spelled the way `mm ls` spells it.
    fn hop(cycle: &Cycle, motion: Motion) -> Option<String> {
        cycle
            .step(motion)
            .map(|next| format!("{}/{}", next.host, next.session))
    }

    /// A rename from inside the session moves where you are, and leaves where
    /// you came from alone: nobody hopped.
    #[test]
    fn a_rename_moves_where_you_are_and_not_where_you_came_from() {
        let mut cycle = cycle(&["api", "build", "web"], "api");
        cycle.moved_to(Located::new("here", "build"));
        cycle.renamed("nightly");
        assert_eq!(cycle.current().session, "nightly");
        assert_eq!(step(&cycle, Motion::Last).as_deref(), Some("api"));
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
    fn tab_walks_this_machine_in_the_order_mm_ls_shows_it() {
        let mut cycle = Cycle::new(Located::new("here", "api"));
        cycle.refresh(vec![
            Located::new("here", "web"),
            Located::new("gpu-box", "train"),
            Located::new("here", "api"),
            Located::new("here", "db"),
        ]);
        // Sorted by machine, then name, so this machine reads api, db, web.
        assert_eq!(hop(&cycle, Motion::Next).as_deref(), Some("here/db"));
        // Around the end of this machine rather than into gpu-box.
        assert_eq!(hop(&cycle, Motion::Previous).as_deref(), Some("here/web"));
    }

    #[test]
    fn tab_stays_on_the_machine_you_are_on() {
        let sessions = &[("box", "one"), ("box", "two"), ("here", "api")];
        let cycle = spread(sessions, ("here", "api"));
        // The only session here, so there is nowhere to tab to: `h` is how you
        // reach box, not the end of the list.
        assert_eq!(hop(&cycle, Motion::Next), None);
        assert_eq!(hop(&cycle, Motion::Previous), None);

        let on_box = spread(sessions, ("box", "two"));
        assert_eq!(hop(&on_box, Motion::Next).as_deref(), Some("box/one"));
    }

    #[test]
    fn the_host_keys_land_on_the_first_session_there() {
        let cycle = spread(
            &[("box", "zulu"), ("box", "alpha"), ("here", "api")],
            ("here", "api"),
        );
        assert_eq!(hop(&cycle, Motion::NextHost).as_deref(), Some("box/alpha"));
    }

    #[test]
    fn the_host_keys_wrap_around_the_machines() {
        let sessions = &[("a", "one"), ("b", "one"), ("c", "one")];
        let at_the_end = spread(sessions, ("c", "one"));
        assert_eq!(hop(&at_the_end, Motion::NextHost).as_deref(), Some("a/one"));
        let at_the_start = spread(sessions, ("a", "one"));
        assert_eq!(
            hop(&at_the_start, Motion::PreviousHost).as_deref(),
            Some("c/one")
        );
    }

    #[test]
    fn the_only_machine_there_is_has_no_next_one() {
        // Not the first session on it, so a host key that came back round would
        // move you somewhere you did not ask to go.
        let cycle = cycle(&["api", "build", "web"], "build");
        assert_eq!(step(&cycle, Motion::NextHost), None);
        assert_eq!(step(&cycle, Motion::PreviousHost), None);
    }

    #[test]
    fn a_machine_that_has_dropped_out_of_the_listing_starts_from_the_end_you_want() {
        let sessions = &[("box", "one"), ("here", "api")];
        let cycle = spread(sessions, ("gone", "old"));
        assert_eq!(hop(&cycle, Motion::NextHost).as_deref(), Some("box/one"));
        assert_eq!(
            hop(&cycle, Motion::PreviousHost).as_deref(),
            Some("here/api")
        );
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
