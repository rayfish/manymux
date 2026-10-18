//! Which sessions you are treating as one piece of work.
//!
//! A group is a name and a set of live sessions, spanning machines, and a
//! session is in at most one. It lives exactly as long as its sessions do:
//! there is no such thing as an empty group, which is what keeps this to a file
//! that needs no tidying and leaves no question about what an empty one would
//! have meant.
//!
//! One file, and every client on this machine is reading and writing it: two
//! `mm attach` runs in two windows, an `mm ls` between them. So it is read
//! before every write and never written from the copy in hand
//! ([`Groups::update`], the only writer there is). An attach reads this once
//! and keeps it for hours while the window next door goes on changing it, and
//! `save` serialised the whole map: the older copy did not merge with what had
//! arrived since, it replaced it, so a session moved into a group in one window
//! fell out of it the next time any other window listed. That is also why the
//! attach loop reads it again on the way round rather than watching it. Nothing
//! in an attached screen is drawn from this file while no key is being pressed,
//! so a watch would have nothing to push to, and the file is small enough to
//! read once per keystroke that goes anywhere.
//!
//! Writers lock a separate file across the read, change and rename. A listing
//! may arrive long after its host answered, so pruning also carries the groups
//! read before asking: it can remove only memberships already present then,
//! in the same group. New sessions and moves made while it was out survive.
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
//! number. The host is stored the way a listing spells it (`hosts::as_listed`):
//! `local` and this machine's own name are one machine, and a member written
//! under the word typed matched no listing row, so it was in no group anyone
//! could see and was never pruned.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::config;
use crate::hosts::as_listed;
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
        Self {
            host: as_listed(host).to_string(),
            pid: session.pid,
            started: seconds(session.started),
        }
    }
}

/// Whole seconds since the epoch, and the epoch itself for a time before it.
///
/// What this file and the checkpoint both write a time down as. A clock set
/// before 1970 is no reason to fail a write, and the epoch is already what a
/// host too old to say sends.
pub fn seconds(at: SystemTime) -> u64 {
    at.duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs())
        .unwrap_or(0)
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
        Self::read(&Self::path())
    }

    fn read(path: &Path) -> Result<Self> {
        config::load_toml(path)
    }

    fn write_to(&self, path: &Path) -> Result<()> {
        config::save_toml(path, self, "the group list")
    }

    /// Read the file, change it, and write it back, in that order.
    ///
    /// The one way to write this file, because the copy in hand is not the
    /// file: an attach reads it once and keeps it for hours, while the other
    /// terminal on the same machine is reading and writing the same path the
    /// whole time. `save` alone serialises the whole map, so an older copy
    /// writing itself back does not merge with what arrived since, it replaces
    /// it, and a session moved into a group in one window fell out of it the
    /// next time any other window listed. Reading first also leaves the caller
    /// holding the file as it is now, which is the copy it goes on to draw.
    ///
    /// The lock is on a separate file, since writing replaces the group file
    /// itself. It covers the read as well as the write so concurrent clients
    /// cannot overwrite each other's changes or share the temporary file.
    pub fn update<T>(&mut self, change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        self.update_at(&Self::path(), change)
    }

    /// The same, for a caller with no copy to go on drawing afterwards.
    pub fn change<T>(change: impl FnOnce(&mut Self) -> Result<T>) -> Result<T> {
        Self::default().update(change)
    }

    fn update_at<T>(
        &mut self,
        path: &Path,
        change: impl FnOnce(&mut Self) -> Result<T>,
    ) -> Result<T> {
        let _lock = Self::lock_at(path)?;
        *self = Self::read(path)?;
        let answer = change(self)?;
        self.write_to(path)?;
        Ok(answer)
    }

    fn lock_at(path: &Path) -> Result<File> {
        if let Some(parent) = path.parent() {
            config::ensure_private_dir(parent)?;
        }
        let path = path.with_extension("lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(file)
    }

    /// Merge a listing's removals without dropping memberships added since
    /// the caller read `before`, which must happen before asking the hosts.
    pub fn refresh(
        &mut self,
        before: &Self,
        reached: &[String],
        listing: &[HostedSession],
    ) -> Result<()> {
        self.refresh_at(&Self::path(), before, reached, listing)
    }

    fn refresh_at(
        &mut self,
        path: &Path,
        before: &Self,
        reached: &[String],
        listing: &[HostedSession],
    ) -> Result<()> {
        let mut survivors = before.clone();
        survivors.prune(reached, listing);
        self.update_at(path, |current| {
            for (name, previous) in &before.groups {
                if let Some(held) = current.groups.get_mut(name) {
                    held.members.retain(|member| {
                        !previous.members.contains(member)
                            || survivors
                                .groups
                                .get(name)
                                .is_some_and(|group| group.members.contains(member))
                    });
                }
            }
            current.forget_the_empty();
            Ok(())
        })
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

    /// Move a session into a group, out of whatever it was in, and answer with
    /// the name that stuck.
    ///
    /// The name is what `@name` has to spell, so the two characters that
    /// spelling is made of cannot be in it. A group called `@pi` was listed on
    /// every screen and reachable from none of them, since `mm a @pi` looks for
    /// a group called `pi` and only `@@pi` finds it; one with a `/` in it is
    /// read as a machine and a session. A leading `@` is taken off rather than
    /// refused, because `@pi` is how the headings and every target spell it and
    /// typing that back at a prompt asking for a name is the obvious thing to
    /// do. It answers with the name for the same reason `Registry::rename`
    /// does: what was typed and what it ended up called are not always the same
    /// string.
    pub fn assign(&mut self, group: &str, host: &str, session: &SessionInfo) -> Result<String> {
        let name = group
            .trim()
            .strip_prefix('@')
            .unwrap_or(group.trim())
            .trim();
        if name.is_empty() {
            bail!("a group needs a name");
        }
        if name.contains('/') {
            bail!("a group name cannot hold a `/`: that is how a machine is spelled");
        }
        let name = name.to_string();
        self.clear(host, session);
        self.groups
            .entry(name.clone())
            .or_default()
            .members
            .insert(Member::of(host, session));
        Ok(name)
    }

    /// Put a session in a group, or take it out of every group for `None`.
    ///
    /// The one shape both ways of choosing a group from somewhere else take:
    /// `mm group host/name [group]` and the popup's move list each hand over
    /// a group or nothing.
    pub fn set(&mut self, group: Option<&str>, host: &str, session: &SessionInfo) -> Result<()> {
        match group {
            Some(group) => {
                self.assign(group, host, session)?;
            }
            None => self.clear(host, session),
        }
        Ok(())
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
    /// were away from it. `Listing::answering()` is kept apart from the
    /// sessions for exactly this, and it is that one rather than
    /// `Listing::reached()`, which leaves this machine out because this machine
    /// is never watched: fed that, every local member was kept forever.
    fn prune(&mut self, reached: &[String], listing: &[HostedSession]) {
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

    /// A group is a set of live sessions and nothing else, so one that has lost
    /// its last member has stopped existing rather than become empty.
    fn forget_the_empty(&mut self) {
        self.groups.retain(|_, held| !held.members.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hosts::{LOCAL, this_machine};
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
            node: String::new(),
        }
    }

    fn hosted(host: &str, session: SessionInfo) -> HostedSession {
        HostedSession {
            host: host.to_string(),
            session,
        }
    }

    /// A group name has to survive being spelled `@name`, and the two
    /// characters that spelling is made of were both accepted: `@pi` came out
    /// listed on every screen and reachable from none of them, since `mm a @pi`
    /// looks for a group called `pi`.
    #[test]
    fn a_group_name_cannot_hold_the_sigil_or_the_separator() {
        let build = session("build", 41823, 1000);

        // The sigil is taken off rather than refused: it is how every screen
        // and every target spells the name, so typing it back is no mistake.
        let mut groups = Groups::default();
        assert_eq!(groups.assign("@pi", "box", &build).unwrap(), "pi");
        assert_eq!(groups.group_of("box", &build), Some("pi"));
        assert_eq!(groups.assign("  pi  ", "box", &build).unwrap(), "pi");

        // The separator cannot be, since it is how a machine is spelled.
        let mut groups = Groups::default();
        assert!(groups.assign("a/b", "box", &build).is_err());
        assert!(groups.assign("@", "box", &build).is_err());
        assert!(groups.assign("   ", "box", &build).is_err());
        assert!(groups.names().is_empty(), "and nothing was written");
    }

    /// The whole reason membership is not keyed on the name: a rename moves the
    /// name and must not move the session out of its group.
    #[test]
    fn a_rename_leaves_a_session_in_its_group() {
        let mut groups = Groups::default();
        groups
            .assign("pi", "box", &session("build", 41823, 1000))
            .unwrap();
        assert_eq!(
            groups.group_of("box", &session("nightly", 41823, 1000)),
            Some("pi")
        );
    }

    #[test]
    fn a_session_is_in_at_most_one_group() {
        let mut groups = Groups::default();
        let s = session("build", 41823, 1000);
        groups.assign("pi", "box", &s).unwrap();
        groups.assign("manymux", "box", &s).unwrap();
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
        groups.assign("pi", "box", &s).unwrap();
        groups.clear("box", &s);
        assert_eq!(groups.group_of("box", &s), None);
    }

    /// What both ways of choosing a group hand over: a name puts the session
    /// in it, with the same rules `assign` keeps, and nothing takes it out.
    #[test]
    fn setting_a_group_assigns_and_setting_none_clears() {
        let mut groups = Groups::default();
        let s = session("build", 41823, 1000);
        groups.set(Some("@pi"), "box", &s).unwrap();
        assert_eq!(groups.group_of("box", &s), Some("pi"));
        assert!(groups.set(Some("a/b"), "box", &s).is_err());
        assert_eq!(
            groups.group_of("box", &s),
            Some("pi"),
            "a refused name moves nothing"
        );
        groups.set(None, "box", &s).unwrap();
        assert_eq!(groups.group_of("box", &s), None);
        assert!(groups.names().is_empty());
    }

    /// A reused pid on a machine that rebooted is a different session, which is
    /// the one case the pid alone cannot tell apart.
    #[test]
    fn a_reused_pid_after_a_reboot_is_not_the_same_session() {
        let mut groups = Groups::default();
        groups
            .assign("pi", "box", &session("build", 41823, 1000))
            .unwrap();
        assert_eq!(groups.group_of("box", &session("other", 41823, 9999)), None);
    }

    /// A host too old to stamp `started` sends the epoch for every session, so
    /// there the pid is all there is and all there ever was.
    #[test]
    fn a_host_too_old_to_stamp_a_start_time_identifies_by_pid_alone() {
        let mut groups = Groups::default();
        groups
            .assign("old", "box", &session("build", 7, 0))
            .unwrap();
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
        groups.assign("pi", "box", &gone).unwrap();
        groups.assign("pi", "box", &live).unwrap();
        groups.prune(&["box".into()], &[hosted("box", live.clone())]);
        assert_eq!(groups.group_of("box", &gone), None);
        assert_eq!(groups.group_of("box", &live), Some("pi"));
    }

    /// `mm group local/build work` is this machine by its other spelling, and
    /// every listing names it by its own: kept as typed, the member matched no
    /// row, so it was in no group anybody could see and was never pruned.
    #[test]
    fn a_session_grouped_as_local_is_found_and_pruned_under_this_machine() {
        let mut groups = Groups::default();
        let build = session("build", 41823, 1000);
        groups.assign("work", LOCAL, &build).unwrap();
        assert_eq!(groups.group_of(this_machine(), &build), Some("work"));

        groups.prune(&[this_machine().to_string()], &[]);
        assert_eq!(groups.group_of(this_machine(), &build), None);
        assert!(groups.names().is_empty(), "and the group went with it");
    }

    /// A machine that is asleep has not said its sessions are gone, and pruning
    /// on its silence would empty its groups while you were away.
    #[test]
    fn a_session_on_a_host_that_did_not_answer_is_left_alone() {
        let mut groups = Groups::default();
        let asleep = session("train", 2201, 500);
        groups.assign("pi", "gpu-box", &asleep).unwrap();
        groups.prune(&["box".into()], &[]);
        assert_eq!(groups.group_of("gpu-box", &asleep), Some("pi"));
    }

    /// An empty group cannot exist: it is a set of live sessions and nothing
    /// else, so the last one leaving takes the group with it.
    #[test]
    fn a_group_whose_last_session_ended_is_gone() {
        let mut groups = Groups::default();
        groups
            .assign("pi", "box", &session("build", 41823, 1000))
            .unwrap();
        groups.prune(&["box".into()], &[]);
        assert!(groups.names().is_empty());
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("manymux-groups-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(format!("{name}.toml"))
    }

    /// Two attached clients on one machine share this file, and each read it
    /// when its attach started. Every listing that lands writes the whole file
    /// back, so the older copy put the file the way it was and the session
    /// moved into a group in the other terminal fell out of it.
    #[test]
    fn a_group_made_in_another_client_survives_this_one_writing() {
        let path = temp("another-client");
        let _ = std::fs::remove_file(&path);
        let build = session("build", 41823, 1000);

        // The client that has been attached since before there were any groups.
        let mut older = Groups::read(&path).unwrap();

        // The other terminal moves a session into a group.
        let mut other = Groups::read(&path).unwrap();
        other.assign("pi", "box", &build).unwrap();
        other.write_to(&path).unwrap();

        // A listing lands in the first one, which prunes and saves.
        older
            .update_at(&path, |groups| {
                groups.prune(&["box".into()], &[hosted("box", build.clone())]);
                Ok(())
            })
            .unwrap();

        assert_eq!(
            Groups::read(&path).unwrap().group_of("box", &build),
            Some("pi"),
            "the other client's move was overwritten"
        );
        assert_eq!(
            older.group_of("box", &build),
            Some("pi"),
            "and this client is still drawing the file as it was"
        );
    }

    /// The other half of the same thing: the client writing is moving a
    /// session of its own, and the group the other terminal just made has to
    /// still be there when it is done.
    #[test]
    fn a_move_in_one_client_does_not_undo_a_move_in_the_other() {
        let path = temp("two-moves");
        let _ = std::fs::remove_file(&path);
        let build = session("build", 41823, 1000);
        let api = session("api", 41902, 1001);

        let mut older = Groups::read(&path).unwrap();

        // One terminal puts `build` in a group.
        let mut other = Groups::read(&path).unwrap();
        other.assign("pi", "box", &build).unwrap();
        other.write_to(&path).unwrap();

        // The other, which has never heard of it, puts `api` in one.
        older
            .update_at(&path, |groups| groups.assign("manymux", "box", &api))
            .unwrap();

        let back = Groups::read(&path).unwrap();
        assert_eq!(back.group_of("box", &build), Some("pi"));
        assert_eq!(back.group_of("box", &api), Some("manymux"));
    }

    #[test]
    fn a_delayed_listing_cannot_prune_a_session_grouped_after_it_started() {
        let path = temp("delayed-listing");
        let _ = std::fs::remove_file(&path);
        let mut older = Groups::read(&path).unwrap();
        let before = older.clone();
        let build = session("build", 123, 1000);
        Groups::default()
            .update_at(&path, |groups| groups.assign("work", "box", &build))
            .unwrap();

        older
            .refresh_at(&path, &before, &["box".into()], &[])
            .unwrap();

        assert_eq!(
            Groups::read(&path).unwrap().group_of("box", &build),
            Some("work")
        );
    }

    #[test]
    fn a_refresh_prunes_known_departures_and_keeps_new_and_unreachable_members() {
        let path = temp("refresh-departures");
        let _ = std::fs::remove_file(&path);
        let gone = session("gone", 123, 1000);
        let live = session("live", 124, 1001);
        let new = session("new", 125, 1002);
        let asleep = session("asleep", 126, 1003);
        let mut before = Groups::default();
        before.assign("work", "box", &gone).unwrap();
        before.assign("work", "box", &live).unwrap();
        before.assign("work", "gpu-box", &asleep).unwrap();
        before.write_to(&path).unwrap();
        Groups::default()
            .update_at(&path, |groups| groups.assign("work", "box", &new))
            .unwrap();

        let mut drawn = Groups::default();
        drawn
            .refresh_at(
                &path,
                &before,
                &["box".into()],
                &[hosted("box", live.clone())],
            )
            .unwrap();

        for groups in [drawn, Groups::read(&path).unwrap()] {
            assert_eq!(groups.group_of("box", &gone), None);
            assert_eq!(groups.group_of("box", &live), Some("work"));
            assert_eq!(groups.group_of("box", &new), Some("work"));
            assert_eq!(groups.group_of("gpu-box", &asleep), Some("work"));
        }
    }

    #[test]
    fn a_delayed_listing_keeps_a_membership_moved_to_another_group() {
        let path = temp("refresh-move");
        let _ = std::fs::remove_file(&path);
        let build = session("build", 123, 1000);
        let mut before = Groups::default();
        before.assign("work", "box", &build).unwrap();
        before.write_to(&path).unwrap();
        Groups::default()
            .update_at(&path, |groups| groups.assign("other", "box", &build))
            .unwrap();

        let mut drawn = Groups::default();
        drawn
            .refresh_at(&path, &before, &["box".into()], &[])
            .unwrap();

        assert_eq!(drawn.group_of("box", &build), Some("other"));
        assert_eq!(
            Groups::read(&path).unwrap().group_of("box", &build),
            Some("other")
        );
    }

    #[test]
    fn concurrent_group_updates_keep_both_changes() {
        use std::sync::mpsc;
        use std::thread;

        let path = temp("concurrent-updates");
        let _ = std::fs::remove_file(&path);
        let build = session("build", 123, 1000);
        let api = session("api", 124, 1001);
        let (entered, entering) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let (finished, finishing) = mpsc::channel();

        thread::scope(|scope| {
            let first_path = &path;
            let first_build = &build;
            let first = scope.spawn(move || {
                Groups::default().update_at(first_path, |groups| {
                    groups.assign("work", "box", first_build)?;
                    entered.send(()).unwrap();
                    released.recv().unwrap();
                    Ok(())
                })
            });
            entering.recv().unwrap();
            let second = scope.spawn(|| {
                let result =
                    Groups::default().update_at(&path, |groups| groups.assign("work", "box", &api));
                finished.send(()).unwrap();
                result
            });
            // An unlocked writer completes here and is overwritten by the first.
            let _ = finishing.recv_timeout(Duration::from_millis(100));
            release.send(()).unwrap();
            first.join().unwrap().unwrap();
            second.join().unwrap().unwrap();
        });

        let back = Groups::read(&path).unwrap();
        assert_eq!(back.group_of("box", &build), Some("work"));
        assert_eq!(back.group_of("box", &api), Some("work"));
    }

    #[test]
    fn the_group_file_round_trips() {
        let mut groups = Groups::default();
        groups
            .assign("pi", "box", &session("build", 41823, 1000))
            .unwrap();
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
        groups.assign("pi", "box", &a).unwrap();
        groups.assign("pi", "box", &b).unwrap();
        groups.assign("manymux", "gpu-box", &c).unwrap();
        let listing = vec![hosted("box", a), hosted("box", b), hosted("gpu-box", c)];
        assert_eq!(
            groups.tally(&listing),
            vec![("manymux".to_string(), 1), ("pi".to_string(), 2)]
        );
    }
}
