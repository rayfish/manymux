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
//!
//! The one move here that no key makes is falling back: a session that ends
//! hands you the one you came from, which is the trail `Motion::Last` walks and
//! so belongs beside it rather than in the caller.
//!
//! The order is the one the listing arrives in, which is by machine and then by
//! when each session was opened, oldest first. It used to be by name, and a
//! name is the one thing about a session that moves: renaming one shuffled the
//! cycle under whoever was tabbing through it, so the key that had been
//! reaching the session next door quietly started reaching a different one.

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

/// A session in the listing, with the group it is in.
///
/// The group is resolved before the cycle sees anything, so nothing here has to
/// learn what a pid is: a [`Located`] stays an address, which is what lets a
/// rename move a session's name without moving it in the cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at: Located,
    pub group: Option<String>,
}

impl Entry {
    pub fn new(at: Located, group: Option<String>) -> Self {
        Self { at, group }
    }
}

/// The sessions a switch key can reach, and where in them you are.
///
/// Two levels, because that is how the sessions are arranged: the sessions on
/// one machine, and the machines. `Motion::Next` walks the first and
/// `Motion::NextHost` the second.
///
/// A third thing narrows both: the focus. While a group is active the cycle
/// sees only its sessions, and every motion keeps its meaning applied to what
/// is left. A group is the work rather than the machine, so tab crosses
/// machines inside one.
///
/// The listing is a snapshot the caller refreshes; it is deliberately allowed
/// to be stale, because a keystroke must not wait on a machine that is asleep.
pub struct Cycle {
    /// Every session on every watched machine, in listing order.
    sessions: Vec<Entry>,
    current: Located,
    /// Where the last hop came from, for [`Motion::Last`].
    previous: Option<Located>,
    /// The group the cycle is narrowed to, if any.
    focus: Option<String>,
}

impl Cycle {
    pub fn new(current: Located) -> Self {
        Self {
            sessions: Vec::new(),
            current,
            previous: None,
            focus: None,
        }
    }

    pub fn current(&self) -> &Located {
        &self.current
    }

    /// Narrow to a group, or to everything.
    ///
    /// State of its own rather than read off the current session: widening out
    /// of a group you are still sitting in has to be possible, and the popup's
    /// `all` row would otherwise have nothing to set.
    pub fn focus(&mut self, group: Option<String>) {
        self.focus = group;
    }

    pub fn focused(&self) -> Option<&str> {
        self.focus.as_deref()
    }

    /// The sessions a motion may land on: the group's when one is active, all
    /// of them otherwise.
    fn visible(&self) -> Vec<&Entry> {
        match &self.focus {
            None => self.sessions.iter().collect(),
            Some(group) => self
                .sessions
                .iter()
                .filter(|entry| entry.group.as_deref() == Some(group.as_str()))
                .collect(),
        }
    }

    /// Take a new listing, in the order it was listed in, so that walking the
    /// cycle matches what you would have read off the `mm ls` table.
    ///
    /// The order is not worked out here because it cannot be: it is by the
    /// time each session was opened, and a [`Located`] is an address rather
    /// than a session, on purpose. Two sessions that swapped names are the same
    /// two places in the cycle, and one that was renamed has not moved at all,
    /// which is the whole point of ordering by the one thing that never
    /// changes.
    pub fn refresh(&mut self, sessions: Vec<Entry>) {
        self.sessions = sessions;
        // A group is a set of live sessions and nothing else, so one that has
        // lost its last member has stopped existing. Left focused, the cycle
        // would have nowhere at all to go and every switch key would do
        // nothing, with no way to find out why.
        if let Some(group) = &self.focus
            && !self
                .sessions
                .iter()
                .any(|entry| entry.group.as_deref() == Some(group.as_str()))
        {
            self.focus = None;
        }
    }

    /// Where a motion lands, or `None` when it lands where you already are.
    pub fn step(&self, motion: Motion) -> Option<Located> {
        let next = match motion {
            Motion::Last => self.previous.clone()?,
            Motion::Next | Motion::Previous => {
                // With no group active, only this machine's sessions: tab is
                // for moving inside the work you are already in, and `h` is for
                // leaving it. With one, the group *is* the work you are in,
                // wherever it happens to be running, so tab crosses machines.
                let visible = self.visible();
                let here: Vec<&Entry> = match self.focus {
                    Some(_) => visible,
                    None => visible
                        .into_iter()
                        .filter(|entry| entry.at.host == self.current.host)
                        .collect(),
                };
                let at = here.iter().position(|entry| entry.at == self.current);
                let index = wrapped(motion == Motion::Next, at, here.len())?;
                here[index].at.clone()
            }
            Motion::NextHost | Motion::PreviousHost => {
                // The machines in listing order, which is the order the runs
                // above appear in. Each is taken where it first appears rather
                // than wherever it stops being the last one seen, so a listing
                // that arrived with a machine's sessions split up is a cycle
                // that is merely in an odd order, not one with a machine in it
                // twice.
                let visible = self.visible();
                let mut hosts: Vec<&str> = Vec::new();
                for entry in &visible {
                    if !hosts.contains(&entry.at.host.as_str()) {
                        hosts.push(&entry.at.host);
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
                visible.iter().find(|e| e.at.host == host)?.at.clone()
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
    ///
    /// The listing is corrected in place rather than left to the next refresh,
    /// which may be a while: one is only asked for after a hop. An entry under
    /// the old name is one the cycle cannot find you at, and not being in the
    /// listing is not the same as being at its start, so the next tab would
    /// have gone back to the first session on the machine.
    pub fn renamed(&mut self, name: &str) {
        let was = std::mem::replace(&mut self.current.session, name.to_string());
        for entry in &mut self.sessions {
            if entry.at.host == self.current.host && entry.at.session == was {
                entry.at.session = name.to_string();
            }
        }
    }

    /// The session you are in has ended: go back to the one you came from, and
    /// answer with it. `None` when this run has not hopped at all, which is a
    /// session somebody named on the command line and nowhere to fall back to.
    ///
    /// What is left behind is left behind for good, in the cycle and in
    /// `Motion::Last` both: the way back leads to a session that has exited,
    /// and there is nothing there to come back to.
    pub fn fall_back(&mut self) -> Option<Located> {
        let back = self.previous.take()?;
        let ended = std::mem::replace(&mut self.current, back);
        self.forget(&ended);
        Some(self.current.clone())
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
        self.sessions.retain(|entry| &entry.at != gone);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cycle(names: &[&str], current: &str) -> Cycle {
        let mut cycle = Cycle::new(Located::new("here", current));
        cycle.refresh(
            names
                .iter()
                .map(|n| Entry::new(Located::new("here", *n), None))
                .collect(),
        );
        cycle
    }

    /// A cycle over sessions with groups on some of them.
    fn grouped(sessions: &[(&str, &str, Option<&str>)], current: (&str, &str)) -> Cycle {
        let mut cycle = Cycle::new(Located::new(current.0, current.1));
        cycle.refresh(
            sessions
                .iter()
                .map(|(host, name, group)| {
                    Entry::new(Located::new(*host, *name), group.map(str::to_string))
                })
                .collect(),
        );
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
                .map(|(host, name)| Entry::new(Located::new(*host, *name), None))
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
        // The listing as it arrives: by machine, and oldest first within one,
        // so this machine reads web, api, db whatever they are called.
        cycle.refresh(vec![
            Entry::new(Located::new("gpu-box", "train"), None),
            Entry::new(Located::new("here", "web"), None),
            Entry::new(Located::new("here", "api"), None),
            Entry::new(Located::new("here", "db"), None),
        ]);
        assert_eq!(hop(&cycle, Motion::Next).as_deref(), Some("here/db"));
        // Around the end of this machine rather than into gpu-box.
        assert_eq!(hop(&cycle, Motion::Previous).as_deref(), Some("here/web"));
    }

    /// The reason the listing is ordered by when a session opened: a rename
    /// used to move it, so the key that reached the session next door reached
    /// somewhere else once you had named one of them.
    #[test]
    fn a_rename_leaves_every_session_where_it_was_in_the_cycle() {
        let mut cycle = cycle(&["api", "build", "web"], "build");
        cycle.renamed("zzz-nightly");
        assert_eq!(step(&cycle, Motion::Next).as_deref(), Some("web"));
        assert_eq!(step(&cycle, Motion::Previous).as_deref(), Some("api"));
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
        // The oldest session on the machine, which is the first one listed
        // under it and not the first alphabetically.
        let cycle = spread(
            &[("box", "zulu"), ("box", "alpha"), ("here", "api")],
            ("here", "api"),
        );
        assert_eq!(hop(&cycle, Motion::NextHost).as_deref(), Some("box/zulu"));
    }

    /// A machine appears in the cycle once, at the first session listed under
    /// it, so a listing that came in with a machine's sessions split up is odd
    /// rather than broken.
    #[test]
    fn a_machine_whose_sessions_are_not_together_is_still_one_machine() {
        let cycle = spread(
            &[("box", "one"), ("here", "api"), ("box", "two")],
            ("here", "api"),
        );
        assert_eq!(hop(&cycle, Motion::NextHost).as_deref(), Some("box/one"));
        assert_eq!(
            hop(&cycle, Motion::PreviousHost).as_deref(),
            Some("box/one")
        );
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
    fn a_session_that_ends_falls_back_to_the_one_you_came_from() {
        let mut cycle = cycle(&["api", "build", "web"], "api");
        cycle.moved_to(Located::new("here", "build"));
        assert_eq!(cycle.fall_back(), Some(Located::new("here", "api")));
        assert_eq!(cycle.current().session, "api");
    }

    /// A run that only ever attached to what the command line named: the
    /// session ending there ends the command, so there is nothing to say.
    #[test]
    fn a_run_that_has_not_hopped_has_nowhere_to_fall_back_to() {
        let mut cycle = cycle(&["api", "build"], "api");
        assert_eq!(cycle.fall_back(), None);
        assert_eq!(cycle.current().session, "api");
    }

    /// The session that ended is gone, so it is neither in the cycle nor the
    /// place `Motion::Last` goes: coming back to it is coming back to nothing.
    #[test]
    fn the_session_that_ended_is_neither_in_the_cycle_nor_where_last_goes() {
        let mut cycle = cycle(&["api", "build", "web"], "api");
        cycle.moved_to(Located::new("here", "build"));
        cycle.fall_back();
        assert_eq!(step(&cycle, Motion::Next).as_deref(), Some("web"));
        assert_eq!(step(&cycle, Motion::Last), None);
    }

    /// The whole point of a group: tab leaves the machine, because the work
    /// does. With none active it stays put, which is every test above this one.
    #[test]
    fn tab_walks_a_group_across_machines() {
        let mut cycle = grouped(
            &[
                ("box", "build", Some("pi")),
                ("box", "api", None),
                ("gpu-box", "train", Some("pi")),
            ],
            ("box", "build"),
        );
        cycle.focus(Some("pi".into()));
        assert_eq!(hop(&cycle, Motion::Next).as_deref(), Some("gpu-box/train"));
        // Around the group, never through the session that is not in it.
        assert_eq!(
            hop(&cycle, Motion::Previous).as_deref(),
            Some("gpu-box/train")
        );
    }

    #[test]
    fn the_host_keys_walk_the_machines_a_group_is_on() {
        let mut cycle = grouped(
            &[
                ("box", "build", Some("pi")),
                ("gpu-box", "other", None),
                ("far", "train", Some("pi")),
            ],
            ("box", "build"),
        );
        cycle.focus(Some("pi".into()));
        // gpu-box has nothing in the group, so it is not a machine to go to.
        assert_eq!(hop(&cycle, Motion::NextHost).as_deref(), Some("far/train"));
    }

    #[test]
    fn widening_puts_every_session_back_in_the_cycle() {
        let mut cycle = grouped(
            &[("box", "build", Some("pi")), ("box", "api", None)],
            ("box", "build"),
        );
        cycle.focus(Some("pi".into()));
        assert_eq!(hop(&cycle, Motion::Next), None);
        cycle.focus(None);
        assert_eq!(hop(&cycle, Motion::Next).as_deref(), Some("box/api"));
    }

    /// A group is a set of live sessions, so the last one ending leaves a
    /// narrowing with nothing behind it, where every switch key would do
    /// nothing and there would be no way to find out why.
    #[test]
    fn a_group_that_emptied_clears_the_focus() {
        let mut cycle = grouped(&[("box", "api", Some("pi"))], ("box", "api"));
        cycle.focus(Some("pi".into()));
        cycle.refresh(vec![Entry::new(Located::new("box", "api"), None)]);
        assert_eq!(cycle.focused(), None);
    }

    /// A hop inside a group is a hop: the way back is recorded the same way,
    /// and works after the focus has moved on.
    #[test]
    fn last_comes_back_through_a_group_you_have_left() {
        let mut cycle = grouped(
            &[
                ("box", "build", Some("pi")),
                ("box", "api", Some("manymux")),
            ],
            ("box", "build"),
        );
        cycle.focus(Some("pi".into()));
        cycle.moved_to(Located::new("box", "api"));
        cycle.focus(Some("manymux".into()));
        assert_eq!(hop(&cycle, Motion::Last).as_deref(), Some("box/build"));
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
