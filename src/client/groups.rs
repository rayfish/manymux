//! Which sessions you are treating as one piece of work.
//!
//! A group is a name and a set of live sessions, spanning machines, and a
//! session is in at most one. It lives exactly as long as its sessions do:
//! there is no such thing as an empty group, which is what keeps this to a file
//! that needs no tidying and leaves no question about what an empty one would
//! have meant.
//!
//! Kept here rather than at the node on purpose. A node runs the build it
//! started from until `mm restart`, and for a host reached over ssh nothing
//! says it is stale, so a group defined there would work on some machines and
//! silently not on others, with no way to see which. Defined here it works the
//! moment one binary is replaced, against every machine you can reach. What it
//! costs is that a group made at your desk is not one your phone sees:
//! membership is yours, which is the right reading of a workspace of your
//! attention rather than a property of the session.
//!
//! Membership is keyed on `(host, pid, started)` and never on the name, because
//! a name is the one thing about a session that moves. Keyed by name, renaming
//! a session would drop it out of its group, and a rename typed on another
//! machine would not even be seen. All three fields are stamped once in
//! `Session::spawn` and never written again. `started` is there for the one
//! case the pid alone cannot answer: a machine that rebooted and reused the
//! number.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::proto::{HostedSession, SessionInfo};

/// One session, named the way it will still be named after a rename.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Member {
    pub host: String,
    pub pid: u32,
    /// Seconds since the epoch. A host too old to stamp a start time sends the
    /// epoch itself, which ties all of its sessions and leaves the pid to
    /// identify them, exactly as much as was available before.
    pub started: u64,
}

impl Member {
    pub fn of(host: &str, session: &SessionInfo) -> Self {
        let started = session
            .started
            .duration_since(UNIX_EPOCH)
            .map(|since| since.as_secs())
            .unwrap_or(0);
        Self {
            host: host.to_string(),
            pid: session.pid,
            started,
        }
    }
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
struct Members {
    #[serde(default)]
    members: BTreeSet<Member>,
}

#[derive(Serialize, Deserialize, Default, Debug, Clone)]
pub struct Groups {
    /// A `BTreeMap` so the file keeps a stable order and does not churn in a
    /// diff, the same reason [`crate::hosts::Hosts`] uses a `BTreeSet`.
    #[serde(default)]
    groups: BTreeMap<String, Members>,
}

impl Groups {
    pub fn load() -> Result<Self> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let text = toml::to_string_pretty(self).context("encoding the group list")?;
        config::write_private_file(&Self::path(), text.as_bytes())
    }

    /// Its own file rather than a section in `settings.toml`, because
    /// membership churns every time a session starts or ends and the settings
    /// are hand-edited.
    pub fn path() -> PathBuf {
        config::config_dir().join("groups.toml")
    }

    pub fn group_of(&self, host: &str, session: &SessionInfo) -> Option<&str> {
        let member = Member::of(host, session);
        self.groups
            .iter()
            .find(|(_, held)| held.members.contains(&member))
            .map(|(name, _)| name.as_str())
    }

    /// Move a session into a group, out of whatever it was in.
    pub fn assign(&mut self, group: &str, host: &str, session: &SessionInfo) {
        self.clear(host, session);
        self.groups
            .entry(group.to_string())
            .or_default()
            .members
            .insert(Member::of(host, session));
    }

    pub fn clear(&mut self, host: &str, session: &SessionInfo) {
        let member = Member::of(host, session);
        for held in self.groups.values_mut() {
            held.members.remove(&member);
        }
        self.forget_the_empty();
    }

    /// Drop members that are gone, considering only hosts that answered.
    ///
    /// A machine that is asleep has said nothing about its sessions, and
    /// reading that silence as "they ended" would empty its groups while you
    /// were away from it. `Listing::reached()` is kept apart from the sessions
    /// for exactly this.
    pub fn prune(&mut self, reached: &[String], listing: &[HostedSession]) {
        let live: BTreeSet<Member> = listing
            .iter()
            .map(|hosted| Member::of(&hosted.host, &hosted.session))
            .collect();
        for held in self.groups.values_mut() {
            held.members
                .retain(|member| !reached.contains(&member.host) || live.contains(member));
        }
        self.forget_the_empty();
    }

    /// Group names with how many live sessions each holds, by name.
    pub fn tally(&self, listing: &[HostedSession]) -> Vec<(String, usize)> {
        self.groups
            .iter()
            .filter_map(|(name, held)| {
                let count = listing
                    .iter()
                    .filter(|hosted| {
                        held.members
                            .contains(&Member::of(&hosted.host, &hosted.session))
                    })
                    .count();
                (count > 0).then(|| (name.clone(), count))
            })
            .collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.groups.keys().cloned().collect()
    }

    pub fn has(&self, group: &str) -> bool {
        self.groups.contains_key(group)
    }

    /// A group is a set of live sessions and nothing else, so one that has lost
    /// its last member has stopped existing rather than become empty.
    fn forget_the_empty(&mut self) {
        self.groups.retain(|_, held| !held.members.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Size;
    use std::time::Duration;

    fn session(name: &str, pid: u32, started: u64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            title: name.to_string(),
            command: "zsh".into(),
            pid,
            size: Size::new(80, 24),
            attached: 0,
            idle: 0,
            bells: 0,
            started: UNIX_EPOCH + Duration::from_secs(started),
        }
    }

    fn hosted(host: &str, session: SessionInfo) -> HostedSession {
        HostedSession {
            host: host.to_string(),
            session,
        }
    }

    /// The whole reason membership is not keyed on the name: a rename moves the
    /// name and must not move the session out of its group.
    #[test]
    fn a_rename_leaves_a_session_in_its_group() {
        let mut groups = Groups::default();
        groups.assign("pi", "box", &session("build", 41823, 1000));
        assert_eq!(
            groups.group_of("box", &session("nightly", 41823, 1000)),
            Some("pi")
        );
    }

    #[test]
    fn a_session_is_in_at_most_one_group() {
        let mut groups = Groups::default();
        let s = session("build", 41823, 1000);
        groups.assign("pi", "box", &s);
        groups.assign("manymux", "box", &s);
        assert_eq!(groups.group_of("box", &s), Some("manymux"));
        assert_eq!(
            groups.tally(&[hosted("box", s)]),
            vec![("manymux".to_string(), 1)]
        );
    }

    #[test]
    fn clearing_takes_a_session_out_of_whatever_it_was_in() {
        let mut groups = Groups::default();
        let s = session("build", 41823, 1000);
        groups.assign("pi", "box", &s);
        groups.clear("box", &s);
        assert_eq!(groups.group_of("box", &s), None);
    }

    /// A reused pid on a machine that rebooted is a different session, which is
    /// the one case the pid alone cannot tell apart.
    #[test]
    fn a_reused_pid_after_a_reboot_is_not_the_same_session() {
        let mut groups = Groups::default();
        groups.assign("pi", "box", &session("build", 41823, 1000));
        assert_eq!(groups.group_of("box", &session("other", 41823, 9999)), None);
    }

    /// A host too old to stamp `started` sends the epoch for every session, so
    /// there the pid is all there is and all there ever was.
    #[test]
    fn a_host_too_old_to_stamp_a_start_time_identifies_by_pid_alone() {
        let mut groups = Groups::default();
        groups.assign("old", "box", &session("build", 7, 0));
        assert_eq!(
            groups.group_of("box", &session("renamed", 7, 0)),
            Some("old")
        );
    }

    #[test]
    fn a_session_that_ended_is_pruned() {
        let mut groups = Groups::default();
        let gone = session("build", 41823, 1000);
        let live = session("api", 41902, 1001);
        groups.assign("pi", "box", &gone);
        groups.assign("pi", "box", &live);
        groups.prune(&["box".into()], &[hosted("box", live.clone())]);
        assert_eq!(groups.group_of("box", &gone), None);
        assert_eq!(groups.group_of("box", &live), Some("pi"));
    }

    /// A machine that is asleep has not said its sessions are gone, and pruning
    /// on its silence would empty its groups while you were away.
    #[test]
    fn a_session_on_a_host_that_did_not_answer_is_left_alone() {
        let mut groups = Groups::default();
        let asleep = session("train", 2201, 500);
        groups.assign("pi", "gpu-box", &asleep);
        groups.prune(&["box".into()], &[]);
        assert_eq!(groups.group_of("gpu-box", &asleep), Some("pi"));
    }

    /// An empty group cannot exist: it is a set of live sessions and nothing
    /// else, so the last one leaving takes the group with it.
    #[test]
    fn a_group_whose_last_session_ended_is_gone() {
        let mut groups = Groups::default();
        groups.assign("pi", "box", &session("build", 41823, 1000));
        groups.prune(&["box".into()], &[]);
        assert!(groups.names().is_empty());
    }

    #[test]
    fn the_group_file_round_trips() {
        let mut groups = Groups::default();
        groups.assign("pi", "box", &session("build", 41823, 1000));
        let text = toml::to_string_pretty(&groups).unwrap();
        let back: Groups = toml::from_str(&text).unwrap();
        assert_eq!(
            back.group_of("box", &session("build", 41823, 1000)),
            Some("pi")
        );
    }

    #[test]
    fn a_tally_counts_only_live_sessions_and_is_ordered_by_name() {
        let mut groups = Groups::default();
        let a = session("build", 1, 1000);
        let b = session("api", 2, 1001);
        let c = session("train", 3, 1002);
        groups.assign("pi", "box", &a);
        groups.assign("pi", "box", &b);
        groups.assign("manymux", "gpu-box", &c);
        let listing = vec![hosted("box", a), hosted("box", b), hosted("gpu-box", c)];
        assert_eq!(
            groups.tally(&listing),
            vec![("manymux".to_string(), 1), ("pi".to_string(), 2)]
        );
    }
}
