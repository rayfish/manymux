//! What the machines said, and the rows the popup draws from it.
//!
//! The snapshot is one fan-out's worth of answers and the listing built from it
//! is a tree: a heading per machine, the groups on it under that, and the
//! sessions inside. Here rather than in the binary because none of it is about
//! a command line. It is the one place that turns a listing into rows, and the
//! rules it keeps are the ones that go wrong quietly: which row the cursor opens
//! on, which rows wear a digit, and which spelling of a machine name is the same
//! machine. Every one of those has failed in a way nobody noticed until the key
//! after it did the wrong thing, so they are tested here rather than through a
//! terminal.
//!
//! The ids the rows carry are indices into the `Vec`s beside them, which is how
//! `client::picker` draws a session without ever learning what a machine is.
//! See [`Listed::of`].
//! Reloads keep every issued id through [`Listed::following`], including rows
//! that disappeared. A pending answer and the displayed rows can then name the
//! same things even when Enter wins the race against drawing the answer.
//! The attached session always has a row: a cached answer may predate its
//! creation or rename, so its absence cannot move the opening highlight.
//! Cached sessions stay visible when their host does not answer. A successful
//! answer removes sessions the host no longer lists.
//! Only fresh answers drive switching and group pruning; cached rows are dimmed.
//! A confirmed exit also removes its cached row before the next listing arrives.
//! A confirmed rename updates both fresh and cached rows immediately, preserving
//! the session's identity and group while the next listing is still pending.
//! A rename received on an attach stream also keeps issued ids, and overrides
//! older names in a listing by the session's stable membership identity.

use anyhow::Result;

use crate::client::attach::Rows;
use crate::client::groups::{Groups, Member};
use crate::client::picker::Row;
use crate::client::switch::{Cycle, Entry, Located};
use crate::hosts::{as_listed, is_this_machine, same_machine, this_machine};
use crate::proto::{HostedSession, SessionInfo};
use crate::term;

/// What every machine said it was running, and which machines said anything.
///
/// The whole `SessionInfo` rather than a name, because group membership is
/// keyed on the pid and the start time and the popup draws titles, idle times
/// and bells from it. The hosts that answered come with it because pruning a
/// group must consider only those: a machine that is asleep has said nothing
/// about its sessions, and reading that silence as "they ended" would empty its
/// groups while you were away from it.
#[derive(Clone, Default)]
pub struct Snapshot {
    pub sessions: Vec<HostedSession>,
    /// Last known sessions for this attach run, retaining unreachable hosts.
    /// Kept apart from fresh answers so they cannot prevent group pruning.
    pub cached: Vec<HostedSession>,
    /// Which machines said so, this one included: it is what pruning is
    /// allowed to act on, and it is `Listing::answering` rather than
    /// `Listing::reached`, which leaves this machine out for a different
    /// question entirely.
    pub answered: Vec<String>,
    /// The group file as it was read before the machines were asked, which is
    /// what bounds what this listing may prune ([`Groups::refresh`]): an
    /// answer can arrive long after it was asked for, and a session moved into
    /// a group while it was out must not be taken out again by it.
    pub before: Groups,
}

impl Snapshot {
    /// Apply a confirmed rename without waiting for the next host listing.
    pub fn renamed(&mut self, at: &Located, name: &str) {
        for hosted in self.sessions.iter_mut().chain(&mut self.cached) {
            if same_machine(&hosted.host, &at.host) && hosted.session.name == at.session {
                hosted.session.name = name.to_string();
            }
        }
    }

    /// Keep an attach-confirmed name even when a pending listing predates it.
    pub fn renamed_member(&mut self, member: &Member, name: &str) {
        for hosted in self.sessions.iter_mut().chain(&mut self.cached) {
            if Member::of(&hosted.host, &hosted.session) == *member {
                hosted.session.name = name.to_string();
            }
        }
    }

    /// Remove a session whose exit was observed on its attach stream.
    pub fn forget(&mut self, at: &Located) {
        let keep = |hosted: &HostedSession| {
            !same_machine(&hosted.host, &at.host) || hosted.session.name != at.session
        };
        self.sessions.retain(keep);
        self.cached.retain(keep);
    }

    /// Keep the last picture while hosts are unavailable. An answer replaces
    /// that host's sessions, removing any it confirms have closed.
    pub fn following(mut self, previous: &Self) -> Self {
        let known = if self.cached.is_empty() {
            previous.known().to_vec()
        } else {
            self.cached
        };
        self.cached = self.sessions.clone();
        for hosted in known {
            if self
                .answered
                .iter()
                .any(|host| same_machine(host, &hosted.host))
            {
                continue;
            }
            let member = Member::of(&hosted.host, &hosted.session);
            if !self.sessions.iter().any(|live| {
                Member::of(&live.host, &live.session) == member
                    || (live.host == hosted.host && live.session.name == hosted.session.name)
            }) {
                self.cached.push(hosted);
            }
        }
        self.cached.sort_by(|a, b| {
            a.host
                .cmp(&b.host)
                .then(a.session.started.cmp(&b.session.started))
        });
        self
    }

    fn known(&self) -> &[HostedSession] {
        if self.cached.is_empty() {
            &self.sessions
        } else {
            &self.cached
        }
    }

    fn available(&self, located: &Located) -> bool {
        self.sessions
            .iter()
            .any(|live| live.host == located.host && live.session.name == located.session)
    }

    /// The listing as the switch keys want it: an address, and the group it is
    /// in.
    pub fn entries(&self, groups: &Groups) -> Vec<Entry> {
        self.sessions
            .iter()
            .map(|hosted| {
                Entry::new(
                    Located::new(&hosted.host, &hosted.session.name),
                    groups
                        .group_of(&hosted.host, &hosted.session)
                        .map(str::to_string),
                )
            })
            .collect()
    }

    /// Tidy the group file by what this listing says has ended, and leave
    /// `groups` holding the file as it now is.
    pub fn refresh(&self, groups: &mut Groups) -> Result<()> {
        groups.refresh(&self.before, &self.answered, &self.sessions)
    }

    /// What a machine said about one session, for the pid a group is keyed on.
    pub fn info(&self, at: &Located) -> Option<&SessionInfo> {
        self.sessions
            .iter()
            .find(|hosted| hosted.host == at.host && hosted.session.name == at.session)
            .map(|hosted| &hosted.session)
    }

    /// The current address of a known session, using fresh answers alone.
    pub fn locate(&self, member: &Member) -> Option<Located> {
        self.sessions
            .iter()
            .find(|hosted| Member::of(&hosted.host, &hosted.session) == *member)
            .map(|hosted| Located::new(&hosted.host, &hosted.session.name))
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// The popup's rows, and what each row id means.
///
/// The ids are indices into the two `Vec`s beside the rows, so the half of the
/// client that draws the popup never learns what a host, a group or a pid is:
/// it hands back an id and this looks it up. Built together and replaced
/// together, or an id would index the wrong session.
#[derive(Clone, Default)]
pub struct Listed {
    pub rows: Rows,
    /// What each session row's id means, looked up by that id.
    ///
    /// `CURRENT` first and then the rest in row order, rather than one per
    /// row in the same order: the session the run is in has to wear the same id
    /// in every listing, and it is here even when this listing has never heard
    /// of it.
    pub at: Vec<Located>,
    /// One per group row. The first is `None`, which is "everything" when you
    /// are narrowing and "no group" when you are moving a session.
    pub groups: Vec<Option<String>>,
    /// One per host row: the machine to start a session on, in the spelling a
    /// listing gives it.
    pub hosts: Vec<String>,
}

/// The machine rows and what each one means, which travel together for the
/// reason the session rows and [`Listed::at`] do: a row's id is where its
/// machine sits in `hosts`, so building one without the other would hand back
/// an id naming a different box.
struct Machines {
    rows: Vec<Row>,
    hosts: Vec<String>,
    /// The row for the machine the run is on, which the list opens on.
    at: usize,
}

/// The id the session the run is in wears in every listing. See [`Listed::of`].
const CURRENT: usize = 0;

/// How many rows can wear a digit: `1` to `9`, and no more, because a two-digit
/// row would want a key you press twice and a moment for the client to decide
/// you had finished pressing it. A run longer than that numbers the nine
/// sessions you were in most recently; the rest are still a tab away.
const DIGITS: u8 = 9;

impl Listed {
    /// Keep issued row ids attached to the session across a confirmed rename.
    pub fn renamed(&mut self, was: &Located, name: &str) {
        for (id, at) in self.at.iter_mut().enumerate() {
            if at == was {
                at.session = name.to_string();
                for row in &mut self.rows.sessions {
                    if !row.heading && row.id == id {
                        row.label = name.to_string();
                    }
                }
            }
        }
    }

    /// Whether a displayed session still accepts commands in this answer.
    pub fn available(&self, id: usize) -> bool {
        self.rows
            .sessions
            .iter()
            .any(|row| !row.heading && row.id == id && row.available)
    }

    /// Keep ids from earlier answers for the lifetime of this attach. Removed
    /// rows leave their ids reserved, since the keyboard may still choose one
    /// from the picture drawn before this answer arrived.
    pub fn following(mut self, previous: &Self) -> Self {
        self.at = preserve_ids(&mut self.rows.sessions, &self.at, &previous.at);
        self.groups = preserve_ids(&mut self.rows.groups, &self.groups, &previous.groups);
        self.hosts = preserve_ids(&mut self.rows.hosts, &self.hosts, &previous.hosts);
        self
    }

    /// A tree two deep: a heading per machine, the groups on that machine under
    /// it with their sessions inside, and whatever is in no group last.
    ///
    /// Within every run the order is the listing's, which is oldest first and
    /// never by name, for the reason every other listing has it: a name moves
    /// under a rename and the rows would shuffle beneath a hand walking them.
    /// A group's place is where its oldest session falls, which rests on the
    /// same thing.
    ///
    /// Narrowed to the focused group when there is one, because that is what
    /// every other way of moving around is narrowed to.
    pub fn new(snapshot: &Snapshot, groups: &Groups, hosts: &[String], cycle: &Cycle) -> Self {
        Self::of(snapshot, groups, hosts, cycle.focused(), &cycle.recent())
    }

    /// The same, from the two things about the cycle that matter, so a task
    /// with no cycle of its own can build these while an attach is up.
    ///
    /// `recent` is [`Cycle::recent`]: the sessions this run has been in, most
    /// recent first, so its head is the session the run is in now and the rest
    /// is what the digits are handed out along. It is never empty; an empty one
    /// is a listing with nothing to look out from, and there is no list to draw.
    pub fn of(
        snapshot: &Snapshot,
        groups: &Groups,
        hosts: &[String],
        focus: Option<&str>,
        recent: &[Located],
    ) -> Self {
        let Some(current) = recent.first() else {
            return Self::default();
        };
        let mut rows = Vec::new();
        // The session the run is in is [`CURRENT`] in every listing, whether or
        // not this one has heard of it yet. An id is an index into this, so it
        // moves the moment a machine appears or a session ends, and the popup
        // keeps its cursor across a listing that lands under it *by id*
        // (`Picker::replace`): unpinned, the one row that must never slide out
        // from under the cursor was the one that did, a fuller listing arriving
        // half a second after the box opened and taking the cursor with it.
        let mut at = vec![current.clone()];
        // Every session that is going to be drawn, each beside the group it is
        // in, still in the order the listing arrived: by machine, then oldest
        // first. Which makes each machine's sessions a run, and the run is what
        // the tree is built out of.
        let showing: Vec<(&HostedSession, Option<&str>)> = snapshot
            .known()
            .iter()
            .map(|hosted| (hosted, groups.group_of(&hosted.host, &hosted.session)))
            .filter(|(_, group)| focus.is_none_or(|focus| *group == Some(focus)))
            .collect();

        // The groups first, each one whole. A group spans machines, so it is
        // the machines that break up under it and not the other way round:
        // nested the other way, the one thing a group is for, seeing a piece of
        // work in one place, was the one thing the list would not show.
        //
        // In the order their first session appears, which is by machine and
        // then oldest first, so a group's place moves only when the oldest
        // session in it ends. Ordering by name would shuffle the list under a
        // rename, which is what every listing here is written to avoid.
        let mut drawn: Vec<&str> = Vec::new();
        for (_, group) in &showing {
            let Some(group) = *group else { continue };
            if drawn.contains(&group) {
                continue;
            }
            drawn.push(group);
            rows.push(Row::heading(format!("@{group}")));
            // The machine on a line of its own rather than in front of every
            // name. `host/name` is how a session is addressed, so it was the
            // obvious label, but a real host name is most of the column: with
            // a mesh name and a slash in front of it there was no room left to
            // tell `service-iroh-dev` from `service-iroh-debug`, and which
            // session it is is the one thing the row exists to say.
            let mut machine: Option<&str> = None;
            for (hosted, _) in showing.iter().filter(|(_, g)| *g == Some(group)) {
                if machine != Some(hosted.host.as_str()) {
                    machine = Some(&hosted.host);
                    rows.push(Row::heading(&hosted.host).indent(1));
                }
                Self::session(&mut rows, &mut at, hosted, current, 2);
            }
        }

        // Then whatever is in no group, under the machine it is on, which is
        // the only thing left to gather it by. No heading says "no group": that
        // would name the one thing a group is not, and with the groups above it
        // the rest of the list needs no introduction.
        let mut machine: Option<&str> = None;
        for (hosted, group) in &showing {
            if group.is_some() {
                continue;
            }
            if machine != Some(hosted.host.as_str()) {
                machine = Some(&hosted.host);
                rows.push(Row::heading(&hosted.host));
            }
            Self::session(&mut rows, &mut at, hosted, current, 1);
        }
        // The cursor opens on the session you are looking out from, always.
        // An answer can predate a successful attach, especially after starting
        // or renaming a session. Keep a row for it until the listing catches
        // up, even if its machine answered: the live attach is newer evidence.
        let listed_here = rows.iter().any(|row| !row.heading && row.id == CURRENT);
        if !listed_here {
            // Under the machine it is on, in the shape the rest of the list
            // uses: a heading, and the session a step in from it.
            let deep = u16::from(focus.is_some());
            rows.push(Row::heading(&current.host).indent(deep));
            rows.push(
                Row::new(CURRENT, &current.session)
                    // The mark every listing puts on the session you are in.
                    // What it is doing is the machine's to say, and it has not
                    // said anything yet.
                    .note("●")
                    .indent(deep + 1),
            );
        }
        for row in rows
            .iter_mut()
            .filter(|row| !row.heading && row.id != CURRENT)
        {
            let located = &at[row.id];
            row.available = snapshot.available(located);
            if !row.available {
                row.note = "n/a".to_string();
            }
        }
        // The digits, handed out along the trail rather than down the box: the
        // list is in listing order, which is the order the sessions were
        // started in, and what a digit is for is going back to where you were.
        //
        // Only to rows that are here. A session that has ended, or that the
        // narrowing is hiding, is skipped rather than spending its number, or
        // the gap would be a key doing nothing in the middle of the ones that
        // work. Which is why this counts rather than indexing `recent`.
        let mut digit = 0;
        for was in recent {
            let Some(id) = at.iter().position(|listed| listed == was) else {
                continue;
            };
            let Some(row) = rows.iter_mut().find(|row| !row.heading && row.id == id) else {
                continue;
            };
            digit += 1;
            row.number = Some(digit);
            if digit == DIGITS {
                break;
            }
        }

        let highlight = rows
            .iter()
            .position(|row| !row.heading && row.id == CURRENT)
            .unwrap_or(0);

        // The first row is "everything" or "no group" depending on which verb
        // opened the list, which is why it carries no name.
        let mut group_rows = vec![Row::new(0, "(none)")];
        let mut names: Vec<Option<String>> = vec![None];
        let held = groups.tally(&snapshot.sessions);
        for (name, count) in held {
            let mark = if focus == Some(name.as_str()) {
                "●"
            } else {
                ""
            };
            group_rows.push(
                // Spelt the way it is typed and the way the session list
                // heads it, so the same thing is not two things across two
                // lists one key apart.
                Row::new(names.len(), format!("@{name}"))
                    .detail(count.to_string())
                    .note(mark),
            );
            names.push(Some(name));
        }

        let machines = Self::machines(snapshot, hosts, current);

        Self {
            rows: Rows {
                sessions: rows,
                groups: group_rows,
                hosts: machines.rows,
                at: highlight,
                machine: machines.at,
                narrowed: focus.map(str::to_string),
            },
            at,
            groups: names,
            hosts: machines.hosts,
        }
    }

    /// The machines a session could be started on, and the row the list opens
    /// on.
    ///
    /// The host list rather than what the last listing heard back from: a
    /// machine that was asleep half a second ago is still a machine you meant
    /// to start something on, and leaving it out makes the key quietly unable
    /// to reach it. What the listing decides is the detail beside each name,
    /// which is what a machine that said nothing has none of.
    ///
    /// In one order with this machine among the rest rather than pinned to the
    /// top, because that is the order the session list's headings are in and
    /// two lists one key apart must not disagree about where a machine sits.
    ///
    /// The machine the run is on is in it whether or not it is watched, and so
    /// is this one. Naming a session outright asks nothing of the host list
    /// (`mm attach box/build` on a machine nobody added, or `deploy@box`, which
    /// is a different node from `box` and rightly a row of its own), and a list
    /// with no row for where you are standing has no way back to it: the
    /// highlight falls to the first row, nothing wears the mark, and the Enter
    /// that used to start a session beside you starts one on a stranger's
    /// machine instead. Which is the failure [`as_listed`] was written for, one
    /// list over, and the same spelling answers it.
    fn machines(snapshot: &Snapshot, hosts: &[String], current: &Located) -> Machines {
        let mut names: Vec<String> = hosts.to_vec();
        names.push(as_listed(&current.host).to_string());
        if !names.iter().any(|host| is_this_machine(host)) {
            names.push(this_machine().to_string());
        }
        names.sort();
        names.dedup();
        let mut rows = Vec::new();
        for name in &names {
            let answered = snapshot
                .answered
                .iter()
                .any(|host| same_machine(host, name));
            let held = snapshot
                .sessions
                .iter()
                .filter(|hosted| same_machine(&hosted.host, name))
                .count();
            let detail = match (answered, held) {
                (false, _) => "no answer".to_string(),
                (true, 0) => "no sessions".to_string(),
                (true, 1) => "1 session".to_string(),
                (true, held) => format!("{held} sessions"),
            };
            let here = same_machine(name, &current.host);
            rows.push(
                Row::new(rows.len(), name)
                    .detail(detail)
                    // The mark every list here puts on where you are.
                    .note(if here { "●" } else { "" }),
            );
        }
        let at = rows
            .iter()
            .position(|row| same_machine(&names[row.id], &current.host))
            .unwrap_or(0);
        Machines {
            rows,
            hosts: names,
            at,
        }
    }

    /// One session row, and the address that goes with it.
    ///
    /// The two go in together and never apart: the row's id is where the
    /// address sits in `at`, which is how the popup can hand back a row without
    /// ever being told what a machine is. The session the run is in is the one
    /// whose address is already there, under [`CURRENT`].
    fn session(
        rows: &mut Vec<Row>,
        at: &mut Vec<Located>,
        hosted: &HostedSession,
        current: &Located,
        indent: u16,
    ) {
        let here = *current == Located::new(&hosted.host, &hosted.session.name);
        let mut note = term::duration(hosted.session.idle);
        if hosted.session.bells > 0 {
            note = format!("{note} *");
        }
        if here {
            note = "●".to_string();
        }
        rows.push(
            // The name alone: whatever heading gathered it, machine included,
            // is a line above it.
            Row::new(if here { CURRENT } else { at.len() }, &hosted.session.name)
                .detail(&hosted.session.title)
                .note(note)
                .indent(indent),
        );
        if !here {
            at.push(Located::new(&hosted.host, &hosted.session.name));
        }
    }
}

fn preserve_ids<T: Clone + PartialEq>(rows: &mut [Row], values: &[T], previous: &[T]) -> Vec<T> {
    let mut kept = previous.to_vec();
    for row in rows.iter_mut().filter(|row| !row.heading) {
        let value = &values[row.id];
        row.id = match kept.iter().position(|before| before == value) {
            Some(id) => id,
            None => {
                let id = kept.len();
                kept.push(value.clone());
                id
            }
        };
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::slice::from_ref;

    use crate::client::picker::Picker;
    use crate::hosts::LOCAL;
    use crate::proto::Size;

    #[test]
    fn a_pending_reload_keeps_the_group_id_a_displayed_choice_uses() {
        let mut first = hosted_on("gpu-box", "build");
        first.session.pid = 1;
        let mut second = hosted_on("gpu-box", "test");
        second.session.pid = 2;
        let mut groups = Groups::default();
        groups.assign("alpha", &first.host, &first.session).unwrap();
        groups
            .assign("work", &second.host, &second.session)
            .unwrap();
        let current = Located::new("gpu-box", "test");
        let before = Snapshot {
            sessions: vec![first, second.clone()],
            answered: vec![],
            ..Snapshot::default()
        };
        let displayed = Listed::of(&before, &groups, &[], None, from_ref(&current));
        let chosen = displayed
            .rows
            .groups
            .iter()
            .find(|row| row.label == "@work")
            .unwrap()
            .id;
        let after = Snapshot {
            sessions: vec![second],
            answered: vec![],
            ..Snapshot::default()
        };
        let pending =
            Listed::of(&after, &groups, &[], None, from_ref(&current)).following(&displayed);
        assert_eq!(pending.groups.get(chosen), Some(&Some("work".into())));
        let mut popup = Picker::new("groups", "choose", displayed.rows.groups, chosen);
        popup.replace(pending.rows.groups, 0);
        assert_eq!(popup.chosen().unwrap().label, "@work");
        assert_eq!(popup.chosen().unwrap().id, chosen);
    }

    #[test]
    fn reloads_reserve_removed_session_and_host_ids() {
        let current = Located::new("gpu-box", "test");
        let first = Snapshot {
            sessions: vec![
                hosted_on("gpu-box", "test"),
                hosted_on("build-box", "build"),
            ],
            answered: vec![],
            ..Snapshot::default()
        };
        let hosts = vec!["gpu-box".into(), "build-box".into()];
        let displayed = Listed::of(&first, &Groups::default(), &hosts, None, from_ref(&current));
        let chosen = displayed
            .rows
            .sessions
            .iter()
            .find(|row| row.label == "build")
            .unwrap()
            .id;
        let chosen_host = displayed
            .rows
            .hosts
            .iter()
            .find(|row| row.label == "build-box")
            .unwrap()
            .id;
        let second = Snapshot {
            sessions: vec![hosted_on("gpu-box", "test"), hosted_on("gpu-box", "new")],
            answered: vec![],
            ..Snapshot::default()
        };
        let pending = Listed::of(
            &second,
            &Groups::default(),
            &["gpu-box".into()],
            None,
            from_ref(&current),
        )
        .following(&displayed);
        assert_eq!(pending.at[chosen], Located::new("build-box", "build"));
        assert_eq!(pending.hosts[chosen_host], "build-box");
        assert_ne!(
            pending
                .rows
                .sessions
                .iter()
                .find(|row| row.label == "new")
                .unwrap()
                .id,
            chosen
        );
        let next = Listed::of(&first, &Groups::default(), &hosts, None, from_ref(&current))
            .following(&pending);
        assert_eq!(
            next.rows
                .sessions
                .iter()
                .find(|row| row.label == "build")
                .unwrap()
                .id,
            chosen
        );
        assert_eq!(
            next.rows
                .hosts
                .iter()
                .find(|row| row.label == "build-box")
                .unwrap()
                .id,
            chosen_host
        );
    }

    fn hosted(name: &str) -> HostedSession {
        HostedSession {
            host: this_machine().to_string(),
            session: SessionInfo {
                name: name.to_string(),
                title: name.to_string(),
                command: "zsh".into(),
                pid: 1,
                size: Size::new(80, 24),
                attached: 0,
                idle: 0,
                bells: 0,
                started: std::time::SystemTime::UNIX_EPOCH,
                node: String::new(),
            },
        }
    }

    /// The bug this closes: `mm new` with no host names this machine `local`,
    /// while the listing the popup is drawn from calls it by its own name, so
    /// the session the run was in matched no row. The popup opened on the first
    /// session in the list with Enter over it, and no row wore the mark.
    #[test]
    fn the_popup_opens_on_the_session_the_run_is_in_however_the_machine_was_named() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test")],
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "test");
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, from_ref(&current));
        let row = &listed.rows.sessions[listed.rows.at];
        assert_eq!(row.label, "test");
        // And the same row is the one wearing the mark, since both are the same
        // comparison and a highlight without one reads as a listing gone stale.
        assert_eq!(row.note, "●");
    }

    /// The rows wearing a digit, in the order the digits run.
    fn numbered(listed: &Listed) -> Vec<(&str, u8)> {
        let mut rows: Vec<(&str, u8)> = listed
            .rows
            .sessions
            .iter()
            .filter_map(|row| Some((row.label.as_str(), row.number?)))
            .collect();
        rows.sort_by_key(|(_, number)| *number);
        rows
    }

    fn hosted_on(host: &str, name: &str) -> HostedSession {
        HostedSession {
            host: host.to_string(),
            ..hosted(name)
        }
    }

    #[test]
    fn unavailable_sessions_keep_their_rows_and_highlight_until_the_host_returns() {
        let current = Located::new("build-box", "build");
        let remote = hosted_on("gpu-box", "train");
        let initial = Snapshot {
            sessions: vec![hosted_on("build-box", "build"), remote.clone()],
            answered: vec!["build-box".into(), "gpu-box".into()],
            ..Snapshot::default()
        };
        let groups = Groups::default();
        let displayed = Listed::of(&initial, &groups, &[], None, from_ref(&current));
        let id = displayed
            .at
            .iter()
            .position(|at| at.session == "train")
            .unwrap();
        let mut picker = Picker::new("sessions", "choose", displayed.rows.sessions.clone(), 0);
        picker.point_at(id);

        let missing = Snapshot::default().following(&initial);
        assert!(missing.entries(&groups).is_empty());
        let offline =
            Listed::of(&missing, &groups, &[], None, from_ref(&current)).following(&displayed);
        picker.replace(offline.rows.sessions.clone(), offline.rows.at);
        assert_eq!(picker.chosen().unwrap().id, id);
        assert_eq!(picker.chosen().unwrap().note, "n/a");
        assert!(picker.actionable().is_none());
        assert!(offline.rows.sessions[offline.rows.at].available);

        let mut remote = remote;
        remote.session.title = "training".into();
        let returned = Snapshot {
            sessions: vec![remote],
            answered: vec!["gpu-box".into()],
            ..Snapshot::default()
        }
        .following(&missing);
        let online =
            Listed::of(&returned, &groups, &[], None, from_ref(&current)).following(&offline);
        picker.replace(online.rows.sessions.clone(), online.rows.at);
        let row = picker.actionable().unwrap();
        assert_eq!(row.id, id);
        assert_eq!(row.detail, "training");
        assert_eq!(returned.cached.len(), 2);
    }

    #[test]
    fn sessions_missing_from_a_successful_answer_are_removed_from_the_cache() {
        let mut ended = hosted_on("gpu-box", "test");
        ended.session.pid = 2;
        let build = hosted_on("gpu-box", "build");
        let initial = Snapshot {
            sessions: vec![build.clone(), ended],
            answered: vec!["gpu-box".into()],
            ..Snapshot::default()
        };
        let fresh = Snapshot {
            sessions: vec![build],
            answered: vec!["gpu-box".into()],
            ..Snapshot::default()
        }
        .following(&initial);
        let current = Located::new("gpu-box", "build");
        let listed = Listed::of(&fresh, &Groups::default(), &[], None, from_ref(&current));
        assert!(listed.rows.sessions.iter().all(|row| row.label != "test"));
        assert_eq!(fresh.cached.len(), 1);
        assert_eq!(fresh.sessions.len(), 1);
        assert_eq!(fresh.entries(&Groups::default()).len(), 1);
    }

    #[test]
    fn a_confirmed_exit_is_removed_before_another_host_answer_arrives() {
        let mut snapshot = Snapshot {
            sessions: vec![
                hosted_on("gpu-box", "build"),
                hosted_on("gpu-box", "test"),
                hosted_on("build-box", "test"),
            ],
            ..Snapshot::default()
        }
        .following(&Snapshot::default());
        snapshot.forget(&Located::new("gpu-box", "test"));
        let current = Located::new("gpu-box", "build");
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, from_ref(&current));
        assert!(!listed.at.contains(&Located::new("gpu-box", "test")));
        assert!(listed.at.contains(&Located::new("build-box", "test")));
        assert_eq!(snapshot.sessions.len(), 2);
        let offline = Snapshot::default().following(&snapshot);
        assert_eq!(offline.cached.len(), 2);
        assert!(
            offline
                .cached
                .iter()
                .all(|hosted| { hosted.host != "gpu-box" || hosted.session.name != "test" })
        );
    }

    #[test]
    fn an_empty_host_answer_removes_closed_sessions_without_forgetting_unreachable_hosts() {
        let initial = Snapshot {
            sessions: vec![
                hosted_on("build-box", "build"),
                hosted_on("gpu-box", "train"),
            ],
            answered: vec!["build-box".into(), "gpu-box".into()],
            ..Snapshot::default()
        };
        let offline = Snapshot::default().following(&initial);
        let fresh = Snapshot {
            answered: vec!["gpu-box".into()],
            ..Snapshot::default()
        }
        .following(&offline);
        let current = Located::new("build-box", "build");
        let listed = Listed::of(&fresh, &Groups::default(), &[], None, from_ref(&current));
        assert!(listed.rows.sessions.iter().all(|row| row.label != "train"));
        assert_eq!(fresh.cached.len(), 1);
        assert_eq!(fresh.cached[0].session.name, "build");
        let failed_again = Snapshot::default().following(&fresh);
        assert_eq!(failed_again.cached.len(), 1);
        assert_eq!(failed_again.cached[0].session.name, "build");
    }

    #[test]
    fn renamed_and_replaced_sessions_do_not_leave_duplicate_cached_rows() {
        let mut replaced = hosted_on("gpu-box", "test");
        replaced.session.pid = 2;
        let initial = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build"), replaced],
            ..Snapshot::default()
        };
        let renamed = Snapshot {
            sessions: vec![hosted_on("gpu-box", "test")],
            ..Snapshot::default()
        }
        .following(&initial);
        assert_eq!(renamed.cached.len(), 1);
        assert_eq!(renamed.cached[0].session.name, "test");
        assert_eq!(renamed.cached[0].session.pid, 1);
    }

    #[test]
    fn a_confirmed_rename_keeps_the_popup_row_in_its_group_before_a_reload() {
        let session = hosted_on("gpu-box", "build");
        let mut groups = Groups::default();
        groups
            .assign("work", &session.host, &session.session)
            .unwrap();
        let mut snapshot = Snapshot {
            sessions: vec![session.clone()],
            cached: vec![session],
            ..Snapshot::default()
        };
        let mut cycle = Cycle::new(Located::new("gpu-box", "build"));
        cycle.refresh(snapshot.entries(&groups));
        cycle.focus(Some("work".to_string()));

        snapshot.renamed(cycle.current(), "nightly");
        cycle.renamed("nightly");
        cycle.refresh(snapshot.entries(&groups));
        let listed = Listed::new(&snapshot, &groups, &[], &cycle);
        let rows = &listed.rows.sessions;
        let row = &rows[listed.rows.at];
        assert_eq!(row.label, "nightly");
        assert_eq!(
            row.indent, 2,
            "the renamed row stays under its group and host"
        );
        assert_eq!(rows.len(), 3, "no old row or extra host heading remains");
        assert_eq!(rows[0].label, "@work");
        assert!(cycle.holds(cycle.current()));
        assert_eq!(cycle.focused(), Some("work"));
    }

    #[test]
    fn an_attached_rename_keeps_the_popup_highlight_when_an_older_listing_arrives() {
        let session = hosted_on("gpu-box", "build");
        let member = Member::of(&session.host, &session.session);
        let mut groups = Groups::default();
        groups
            .assign("work", &session.host, &session.session)
            .unwrap();
        let mut snapshot = Snapshot {
            sessions: vec![session],
            ..Snapshot::default()
        }
        .following(&Snapshot::default());
        let old = Located::new("gpu-box", "build");
        let mut previous = Listed::of(&snapshot, &groups, &[], Some("work"), from_ref(&old));
        let id = previous.rows.sessions[previous.rows.at].id;
        let mut picker = Picker::new(
            "sessions",
            "choose",
            previous.rows.sessions.clone(),
            previous.rows.at,
        );

        snapshot.renamed_member(&member, "nightly");
        previous.renamed(&old, "nightly");
        let current = Located::new("gpu-box", "nightly");
        let listed = Listed::of(&snapshot, &groups, &[], Some("work"), from_ref(&current))
            .following(&previous);
        picker.replace(listed.rows.sessions.clone(), listed.rows.at);
        let selected = picker.chosen().unwrap();
        assert_eq!(selected.id, id);
        assert_eq!(selected.label, "nightly");
        assert_eq!(listed.at[id], current);
        assert_eq!(listed.rows.sessions.len(), 3);
        assert_eq!(listed.rows.sessions[0].label, "@work");
        assert_eq!(snapshot.info(&current).unwrap().pid, member.pid);
    }

    #[test]
    fn renaming_another_popup_row_preserves_its_identity_and_other_hosts() {
        let session = hosted_on("gpu-box", "build");
        let other_host = hosted_on("build-box", "build");
        let current = Located::new("gpu-box", "test");
        let mut groups = Groups::default();
        groups
            .assign("work", &session.host, &session.session)
            .unwrap();
        let mut snapshot = Snapshot {
            sessions: vec![session.clone(), other_host.clone()],
            ..Snapshot::default()
        }
        .following(&Snapshot::default());

        snapshot.renamed(&Located::new("gpu-box", "build"), "nightly");
        let renamed = Located::new("gpu-box", "nightly");
        let info = snapshot.info(&renamed).unwrap();
        assert_eq!(info.pid, session.session.pid);
        assert_eq!(info.started, session.session.started);
        assert_eq!(info.title, session.session.title);
        assert_eq!(groups.group_of("gpu-box", info), Some("work"));
        assert!(snapshot.info(&Located::new("gpu-box", "build")).is_none());
        assert_eq!(snapshot.sessions[1].session.name, other_host.session.name);

        let listed = Listed::of(&snapshot, &groups, &[], None, from_ref(&current));
        assert!(listed.at.contains(&renamed));
        assert!(!listed.at.contains(&Located::new("gpu-box", "build")));
    }

    /// What the switch keys walk: every session in the listing, in its order,
    /// each with the group it is in by the membership handed in rather than by
    /// any read before asking.
    #[test]
    fn the_entries_are_the_listing_with_each_session_s_group() {
        let mut build = hosted_on("gpu-box", "build");
        build.session.pid = 1;
        let mut test = hosted_on("gpu-box", "test");
        test.session.pid = 2;
        let mut groups = Groups::default();
        groups.assign("work", &test.host, &test.session).unwrap();
        let snapshot = Snapshot {
            sessions: vec![build, test],
            answered: vec!["gpu-box".to_string()],
            ..Snapshot::default()
        };
        assert_eq!(
            snapshot.entries(&groups),
            vec![
                Entry::new(Located::new("gpu-box", "build"), None),
                Entry::new(Located::new("gpu-box", "test"), Some("work".to_string())),
            ]
        );
    }

    /// A fan-out gets half a second and no more, so on a fleet slower than that
    /// the box opens on whatever landed first, which is not necessarily the
    /// machine you are sitting on. The cursor still opens on the session you
    /// are in: without a row for it, it opened on a stranger's session with
    /// Enter as the next key.
    #[test]
    fn the_popup_opens_on_the_session_the_run_is_in_before_its_machine_has_answered() {
        let snapshot = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build")],
            answered: vec!["gpu-box".to_string()],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "test");
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, from_ref(&current));
        let row = &listed.rows.sessions[listed.rows.at];
        assert_eq!(row.label, "test");
        assert_eq!(row.note, "●");
        // And Enter on it goes to the session the run is in, rather than to
        // whichever session that row id happened to name.
        assert_eq!(listed.at[row.id], current);
    }

    /// A fuller listing arriving under an open box must not take the cursor
    /// with it: an id is an index, and a machine appearing shifts every one of
    /// them, so the row the cursor was on named a different session a moment
    /// later. The session the run is in wears the same id in both listings.
    #[test]
    fn a_listing_landing_under_the_box_keeps_the_cursor_where_the_run_is() {
        let current = Located::new(as_listed(LOCAL), "test");
        let partial = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build")],
            answered: vec!["gpu-box".to_string()],
            ..Snapshot::default()
        };
        let first = Listed::of(&partial, &Groups::default(), &[], None, from_ref(&current));
        let mut popup = Picker::new(
            "sessions",
            "⏎ go",
            first.rows.sessions.clone(),
            first.rows.at,
        );
        let full = Snapshot {
            sessions: vec![hosted_on("gpu-box", "build"), hosted("api"), hosted("test")],
            answered: vec!["gpu-box".to_string(), this_machine().to_string()],
            ..Snapshot::default()
        };
        let then = Listed::of(&full, &Groups::default(), &[], None, from_ref(&current));
        popup.replace(then.rows.sessions.clone(), then.rows.at);
        let row = popup.chosen().expect("a row to be under the cursor");
        assert_eq!(row.label, "test");
        assert_eq!(then.at[row.id], current);
    }

    /// The list the new key opens: every machine you watch, whether or not it
    /// answered the last listing, with the one you are on under the highlight.
    ///
    /// The hosts rather than what the listing heard back from, because a
    /// machine that was asleep half a second ago is still one you meant to
    /// start something on, and a key that cannot reach it is a key that has
    /// quietly stopped covering half the fleet.
    #[test]
    fn the_new_session_list_holds_every_machine_and_opens_on_the_one_you_are_on() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build")],
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "build");
        let hosts = ["gpu-box".to_string()];
        let listed = Listed::of(
            &snapshot,
            &Groups::default(),
            &hosts,
            None,
            from_ref(&current),
        );

        // One order for both lists, this machine among the rest rather than
        // pinned on top: the session list heads its machines the same way.
        let drawn: Vec<&str> = listed
            .rows
            .hosts
            .iter()
            .map(|row| row.label.as_str())
            .collect();
        let mut expected = vec!["gpu-box", this_machine()];
        expected.sort_unstable();
        assert_eq!(drawn, expected);

        let row = &listed.rows.hosts[listed.rows.machine];
        assert_eq!(row.label, this_machine(), "opened on another machine");
        assert_eq!(row.note, "●");
        assert_eq!(listed.hosts[row.id], this_machine());
        // What the listing has to say about each one, which for a machine that
        // said nothing is that it said nothing.
        assert_eq!(row.detail, "1 session");
        let asleep = listed
            .rows
            .hosts
            .iter()
            .find(|row| row.label == "gpu-box")
            .expect("a row for a machine that did not answer");
        assert_eq!(asleep.detail, "no answer");
    }

    /// And the machine the run is on is in it whether or not it is watched.
    ///
    /// The bug this closes: naming a session outright asks nothing of the host
    /// list, so `mm attach box/build` works on a machine nobody added, and
    /// `deploy@box` is a node of its own. Without a row for where you are
    /// standing the highlight fell to the first row, nothing wore the mark, and
    /// the Enter that is supposed to start a session beside you started one on
    /// whichever machine happened to sort first.
    #[test]
    fn the_new_session_list_holds_the_machine_the_run_is_on_however_it_was_named() {
        let snapshot = Snapshot {
            sessions: vec![hosted_on("deploy@box", "build")],
            answered: vec!["deploy@box".to_string()],
            ..Snapshot::default()
        };
        let current = Located::new("deploy@box", "build");
        let hosts = ["gpu-box".to_string()];
        let listed = Listed::of(
            &snapshot,
            &Groups::default(),
            &hosts,
            None,
            from_ref(&current),
        );
        let row = &listed.rows.hosts[listed.rows.machine];
        assert_eq!(row.label, "deploy@box", "opened on another machine");
        assert_eq!(row.note, "●");
        // And Enter on it starts the session there rather than on whichever
        // machine that row id happened to name.
        assert_eq!(listed.hosts[row.id], "deploy@box");
    }

    #[test]
    fn a_cached_answer_cannot_move_the_cursor_off_the_attached_session() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build")],
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        // The attach succeeded after this answer was taken, as when a session
        // was just started or renamed and the next fan-out is still running.
        let current = Located::new(as_listed(LOCAL), "new");
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, from_ref(&current));
        let mut popup = Picker::new(
            "sessions",
            "choose",
            listed.rows.sessions.clone(),
            listed.rows.at,
        );
        let row = popup.chosen().unwrap();
        assert_eq!(listed.at[row.id], current);
        assert_eq!(row.note, "●");
        assert_eq!(row.number, Some(1));

        let fresh = Snapshot {
            sessions: vec![hosted("build"), hosted("new")],
            ..snapshot
        };
        let updated = Listed::of(&fresh, &Groups::default(), &[], None, from_ref(&current))
            .following(&listed);
        popup.replace(updated.rows.sessions.clone(), updated.rows.at);
        assert_eq!(updated.at[popup.chosen().unwrap().id], current);
    }

    /// The digit column, and the whole of what it means: the session you are in
    /// is 1 and the one you came from is 2, so going back is always the same
    /// key however far around the machines you have walked.
    #[test]
    fn the_popup_numbers_the_sessions_you_have_been_in_most_recent_first() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test"), hosted("web")],
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        let recent = [
            Located::new(as_listed(LOCAL), "test"),
            Located::new(as_listed(LOCAL), "build"),
        ];
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, &recent);
        assert_eq!(numbered(&listed), [("test", 1), ("build", 2)]);
    }

    /// A session that has since ended has no row to number, and the digits are
    /// read off the box rather than off the trail: a gap in them would be a key
    /// that does nothing sitting in the middle of the ones that work.
    #[test]
    fn a_session_that_has_ended_leaves_no_gap_in_the_numbers() {
        let snapshot = Snapshot {
            sessions: vec![hosted("build"), hosted("test")],
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        let recent = [
            Located::new(as_listed(LOCAL), "test"),
            Located::new(as_listed(LOCAL), "gone"),
            Located::new(as_listed(LOCAL), "build"),
        ];
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, &recent);
        assert_eq!(numbered(&listed), [("test", 1), ("build", 2)]);
    }

    /// There are nine digits, and a run long enough to walk past them numbers
    /// the nine you were in most recently. The tenth is still in the list and
    /// still reachable with tab; it just has no key of its own.
    #[test]
    fn the_numbering_stops_at_the_last_digit_there_is() {
        let names: Vec<String> = (0..12).map(|n| format!("s{n}")).collect();
        let snapshot = Snapshot {
            sessions: names.iter().map(|n| hosted(n)).collect(),
            answered: vec![this_machine().to_string()],
            ..Snapshot::default()
        };
        let recent: Vec<Located> = names
            .iter()
            .map(|n| Located::new(as_listed(LOCAL), n))
            .collect();
        let listed = Listed::of(&snapshot, &Groups::default(), &[], None, &recent);
        let numbers = numbered(&listed);
        assert_eq!(numbers.len(), 9);
        assert_eq!(numbers.last(), Some(&("s8", 9)));
    }
}
