//! The attached client's loop: attach, and keep attaching.
//!
//! Everything between `mm attach` and the shell it hands back: the hops the
//! switch keys and the popup ask for, the listing they are drawn from, and the
//! wait for a connection that dropped. The terminal itself is
//! `client::attach`'s; this is the half that knows hosts, groups and pids.
//! A transport lost during the attach handshake is unreachable, rather than
//! evidence that the session disappeared from a switch listing.
//! Each attach run retains its last known sessions, dimming unavailable ones
//! while only fresh answers drive switching and group pruning.
//! Listings refresh throughout an attachment, including while the menu is open.
//! A confirmed exit is removed from the cache before falling back.
//! Confirmed renames update the snapshot before rebuilding the popup, so its
//! current row keeps its group while a fresh listing is pending.
//! Popup actions use bounded, hushed connections and never offer installation
//! while the terminal owns stdin.
//! Reconnects recover an unacknowledged rename only when a fresh listing finds
//! the same stable session identity under another name.

use std::collections::HashSet;
use std::fmt::{self, Display, Formatter};
use std::future::Future;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow, bail};
use tokio::task::JoinHandle;
use tracing::debug;

use manymux::client::attach::{self, Chose, Mode, Motion, Outcome, Wait};
use manymux::client::connections::Connections;
use manymux::client::groups::{Groups, Member};
use manymux::client::listed::{Listed, Snapshot};
use manymux::client::switch::{Cycle, Located};
use manymux::client::{Attached, Gone};
use manymux::hosts::{Hosts, as_listed, is_this_machine, qualified};
use manymux::lock::held as held_lock;
use manymux::proto::{Request, Response, SessionInfo, SpawnSpec, session_name};
use manymux::settings::{Screen, Settings};
use manymux::style;

use crate::target::survey_everywhere;
use crate::{FAILED, OK, Reach, current_dir, open_hushed, reach, spawn};

/// How long the first attach waits to learn its initial group. Switches
/// collect only finished listings and do not spend this deadline.
const LISTING_WAIT: Duration = Duration::from_millis(500);

const LISTING_EVERY: Duration = Duration::from_secs(1);

/// The screen to paint on: what was asked for, else what the settings say.
///
/// A flag rather than only a setting because the choice is per attach: trying
/// inline costs nothing and going back is closing the window.
pub(crate) fn chosen(screen: Option<Screen>) -> Screen {
    screen.unwrap_or_else(|| Settings::or_default().screen)
}

/// Attach, and keep attaching: for as long as the switch keys ask for another
/// session, and for as long as one that ends has another to hand back to.
///
/// The terminal is held across the whole run rather than per session, so a hop
/// is a repaint and not a trip through the whole setup and back.
pub(crate) async fn do_attach(
    socket: &Path,
    host: &str,
    name: &str,
    screen: Screen,
    watching: bool,
) -> Result<u8> {
    let mut cycle = Cycle::new(Located::new(as_listed(host), name));
    // Asked for before the first attach, so the first switch key has something
    // to go on.
    let mut listing = Some(spawn_listing(socket));
    // What the machines last said, kept for the pids a group is keyed on.
    let mut snapshot = Snapshot::default();
    let mut groups = Groups::load().unwrap_or_default();
    // Where the popup's lister leaves its answer, so the ids the popup hands
    // back are looked up in the rows it was actually showing, and the loop can
    // take back the snapshot behind them: it hands its own pending listing
    // over, so the answer arrives there and not here.
    let latest: Arc<Mutex<Option<Fresh>>> = Arc::new(Mutex::new(None));

    let mut connections = Connections::default();
    let mut held = attach::hold(screen)?;
    let mut mode = Mode::Focus;
    let mut hopped = false;
    // Sessions whose history this run has already put in the terminal's
    // scrollback. Without this, walking the list with the switch key would dump
    // a thousand lines on every hop.
    let mut seeded: HashSet<String> = HashSet::new();
    // The connection this run is without, while it is without one. Cleared
    // whenever an attach works, which is what makes one that comes back and
    // goes again start over at the quick delays and at a fresh clock.
    let mut lost: Option<Lost> = None;
    // Whether this run has been attached to anything yet, which is what tells
    // a command that did not work from a connection that went.
    let mut attached = false;
    // Something the last key has to say for itself, for the mark row of the
    // attach that follows it. Nothing else here has anywhere to say it: the
    // terminal between two attaches is showing a session.
    let mut notice: Option<String> = None;
    // The exit this run is carrying while it goes back to the session it came
    // from, kept because that fall-back can fail: a session that ended is the
    // end of the run after all if there is no longer anywhere to land.
    let mut ended: Option<(Outcome, String)> = None;
    let mut known: Option<KnownSession> = None;
    let (outcome, where_) = loop {
        if known
            .as_ref()
            .is_some_and(|known| known.at != *cycle.current())
        {
            known = None;
        }
        if known.is_none() {
            known = KnownSession::of(&snapshot, cycle.current());
        }
        take_listing(&mut listing, &mut cycle, &mut snapshot, &mut groups, false).await;
        if let Some(known) = &mut known {
            known.recover(&snapshot, &mut cycle, &mut seeded);
        }
        let target = cycle.current().clone();
        let mut where_ = qualified(&target.host, &target.session);
        let history = if seeded.insert(where_.clone()) {
            screen.history()
        } else {
            0
        };
        let first_attach = !attached;
        let session = match reaching(
            attach_using(socket, &target, history, watching, &mut connections),
            attached,
        )
        .await
        {
            Ok(session) => session,
            // The way back out of a session that ended is gone too, and there
            // is nothing further back than the one you came from. So the run
            // is over after all, and what it has to report is the exit it was
            // carrying rather than this.
            Err(Missed::Gone(e)) if ended.is_some() => {
                debug!("could not fall back to {where_}: {e:#}");
                break ended.take().expect("carrying an exit, by the guard");
            }
            // A hop onto a session that has gone since the listing was taken:
            // the machine answered and has no such session. Stay where you
            // were and put the dead entry out of the cycle, rather than
            // throwing away a working attach over it.
            Err(Missed::Gone(e)) if hopped => {
                debug!("could not attach to {where_}: {e:#}");
                cycle.forget(&target);
                cycle.undo();
                hopped = false;
                continue;
            }
            // Anything else, once this run has been attached to something, is
            // waited out and never reported. The machine being unreachable and
            // the session having gone with the node that held it look the same
            // from here, and both are worth another try: a node restarting is
            // a session that can come back under the same name. Which target
            // is waited for matters, and it is this one: a drop in the moment
            // after a hop must wait for the session hopped to, not walk back
            // to the one the command line named.
            Err(e) if attached => {
                debug!("could not get back to {where_}: {e:#}");
                if listing.is_none() {
                    listing = Some(spawn_listing(socket));
                }
                match wait_to_reconnect(&mut held, &where_, &mut lost, &mut mode).await {
                    Wait::Retry => continue,
                    Wait::GiveUp => break (Outcome::Disconnected, where_),
                }
            }
            // The first attach of the run, which is a command that did not
            // work rather than a connection that went: `mm attach` naming a
            // session nobody is running says so and gives the shell back.
            Err(e) => return Err(e.into()),
        };
        lost = None;
        attached = true;
        // Landed, so the exit that sent us here is somebody else's news now:
        // the next one to report is whatever this session does.
        ended = None;
        // And the hop is over. What the flag above buys is one arm: a hop onto
        // a session the listing was stale about, which is a thing that is true
        // for exactly as long as the attach it describes has not happened.
        // Left set, it was still set an hour later when the node holding the
        // session restarted, and the drop that came back as `Missed::Gone` was
        // read as that stale listing: the cycle forgot a live entry and the run
        // walked back to the session the command line named, which is the one
        // thing the arm below is written to stop.
        hopped = false;

        // The first attach may wait briefly for its initial group. Later
        // landings use cached rows and only collect a finished listing, so
        // a slow machine never delays a switch.
        take_listing(
            &mut listing,
            &mut cycle,
            &mut snapshot,
            &mut groups,
            first_attach,
        )
        .await;
        if let Some(found) = KnownSession::of(&snapshot, cycle.current()) {
            known = Some(found);
        }
        // Narrowing to the group of the session you arrived in, which is a
        // thing to decide on the way in and never afterwards. It used to wait
        // for a listing that said anything, and on a fleet slower than
        // `LISTING_WAIT` the first of those lands well after the run has
        // started: the block then fired on whatever session you were on by
        // then, so putting a session in a group silently narrowed the run to
        // it. Not knowing is an answer here, and the answer is to narrow to
        // nothing.
        // Read again rather than watched. `groups.toml` is shared with every
        // other client on this machine, so the copy this run started with goes
        // stale the moment another terminal moves a session, and this is where
        // the run next looks at it: the rows the popup opens on and the cycle
        // the switch keys walk are both built below. A watch would have nothing
        // to push to, since nothing in an attached screen is drawn from this
        // file while a key is not being pressed, and the file is a few hundred
        // bytes read once per keystroke that goes anywhere. Kept if it will not
        // read, a broken file being no reason to forget what is in hand.
        groups = Groups::load().unwrap_or(groups);
        // Which is only half of it until the cycle is told: the rows below are
        // built from the file just read, and the narrowing they are drawn under
        // is whatever the last listing left behind. So the entries are made
        // again, against the same snapshot and the membership as it now is,
        // which is what lets a session the window next door moved out of this
        // run's group widen the run rather than waiting on a fan-out to say so.
        // Only against a snapshot that says something: an empty one is a run
        // whose listing has never landed, and every group is missing from it.
        if !snapshot.is_empty() {
            cycle.refresh(snapshot.entries(&groups));
        }
        // Landing in a session that is in a group is landing in that work, so
        // the switch keys should walk that work. Decided on the first landing
        // only rather than derived from the current session, because widening
        // out of a group you are still sitting in has to be possible.
        if first_attach && let Some(info) = snapshot.info(cycle.current()) {
            let group = groups
                .group_of(&cycle.current().host, info)
                .map(str::to_string);
            cycle.focus(group);
        }
        // The machines `n` offers, read here for the same reason `groups.toml`
        // is: the file is shared with every other client on this box, and a
        // host added from the window next door should be in the list this
        // attach opens.
        let machines = Hosts::load().map(|hosts| hosts.names()).unwrap_or_default();
        let listed = Listed::new(&snapshot, &groups, &machines, &cycle);
        // The popup's way of asking what the machines are running, since the
        // attach owns this task for as long as it lasts and cannot go and look
        // itself. The narrowing cannot change under it, every way of changing
        // that ending the attach, so it is handed over as it stands. The groups
        // can: `groups.toml` is one file shared by every client on this machine,
        // and the other terminal moving a session while this popup is up is
        // exactly the case, so the copy below is a fallback for a file that
        // will not read rather than the answer.
        let (asks, mut wanted) = tokio::sync::mpsc::channel::<()>(1);
        let (answered, fresh) = tokio::sync::watch::channel(listed.rows.clone());
        let (renamed, mut names) = tokio::sync::watch::channel(None::<String>);
        let lister = {
            let socket = socket.to_path_buf();
            let groups = groups.clone();
            let focus = cycle.focused().map(str::to_string);
            let mut recent = cycle.recent();
            let mut cached = snapshot.clone();
            let mut member = snapshot
                .info(cycle.current())
                .map(|info| Member::of(&target.host, info));
            let latest = Arc::clone(&latest);
            let mut previous = listed.clone();
            // The listing already out there, handed over rather than left to
            // finish into a variable nobody reads again until the attach ends.
            // `take_listing` gives one half a second and no more, so on a fleet
            // where a fan-out takes longer this is *always* still running when
            // the popup opens, and the popup then started a second one from
            // scratch: two ssh commands per host for one keypress, and an empty
            // box for as long as the second took.
            let mut running = listing.take();
            tokio::spawn(async move {
                let mut refresh = tokio::time::interval(LISTING_EVERY);
                refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    let reload = tokio::select! {
                        request = wanted.recv() => {
                            if request.is_none() { return; }
                            true
                        },
                        _ = refresh.tick() => true,
                        changed = names.changed() => {
                            if changed.is_err() { return; }
                            false
                        },
                    };
                    let mut snapshot = if reload {
                        let listing = match running.take() {
                            Some(task) => match task.await {
                                Ok(snapshot) => Ok(snapshot),
                                Err(_) => continue,
                            },
                            None => survey(&socket).await,
                        };
                        let Ok(snapshot) = listing else { continue };
                        snapshot.following(&cached)
                    } else {
                        cached.clone()
                    };
                    let confirmed = names.borrow_and_update().clone();
                    if let Some(current) = recent.first_mut() {
                        let now = Located::new(
                            &current.host,
                            confirmed.as_deref().unwrap_or(&current.session),
                        );
                        if member.is_none() {
                            member = snapshot.info(&now).map(|info| Member::of(&now.host, info));
                        }
                        if let Some(name) = confirmed {
                            if let Some(member) = &member {
                                snapshot.renamed_member(member, &name);
                            } else {
                                snapshot.renamed(current, &name);
                            }
                            previous.renamed(current, &name);
                            current.session = name;
                        }
                    }
                    cached = snapshot.clone();
                    let held = Groups::load().unwrap_or_else(|_| groups.clone());
                    let machines = Hosts::load().map(|hosts| hosts.names()).unwrap_or_default();
                    let listed = Listed::of(&snapshot, &held, &machines, focus.as_deref(), &recent)
                        .following(&previous);
                    previous = listed.clone();
                    // Every answer retains earlier ids, so a choice from
                    // either the displayed rows or this pending answer has
                    // the same meaning here after the attach ends. And the
                    // snapshot behind them, because the loop's own copy is now
                    // the older of the two: it gave up waiting on the listing
                    // this task went on to collect.
                    *held_lock(&latest) = Some(Fresh {
                        listed: listed.clone(),
                        snapshot,
                    });
                    if answered.send(listed.rows).is_err() {
                        return;
                    }
                }
            })
        };
        let uploads = {
            let socket = socket.to_path_buf();
            let host = target.host.clone();
            attach::Uploads::new(move |name, image| {
                let socket = socket.clone();
                let host = host.clone();
                async move {
                    let mut stream = reach(
                        &socket,
                        &host,
                        Reach {
                            start: false,
                            hushed: true,
                            install: false,
                        },
                    )
                    .await?;
                    stream.paste(&name, image.kind, &image.data).await
                }
            })
        };
        let ran = attach::run(
            &mut held,
            session,
            &where_,
            attach::RunOptions {
                mode,
                notice: notice.take().as_deref(),
                popup: attach::PopupFeed {
                    rows: listed.rows.clone(),
                    group: cycle.focused(),
                    asks,
                    fresh,
                    renamed,
                },
                uploads,
            },
        )
        .await;
        lister.abort();
        if let Err(e) = lister.await
            && !e.is_cancelled()
        {
            debug!("popup listing task failed: {e}");
        }
        let attach::Run {
            outcome,
            renamed,
            connection,
        } = ran?;
        if let Some(connection) = connection {
            connections.insert(target.host.clone(), connection);
        }
        // The latest answer includes the meaning of every id issued during
        // this attach, even when its rows had not been drawn before a choice.
        // And what those rows were built from, which the loop gave up waiting
        // for and the popup went on to collect. Without taking it back, a fleet
        // slower than `LISTING_WAIT` never fills this in at all: every attach
        // hands its pending listing to the popup, so nothing is ever left for
        // `take_listing` to find.
        let fresh = held_lock(&latest).take();
        let listed = match fresh {
            Some(Fresh {
                listed,
                snapshot: fresher,
            }) => {
                absorb(fresher, &mut snapshot, &mut cycle, &mut groups);
                listed
            }
            None => listed,
        };
        // Renamed from inside, so the name this run has been using is nobody's
        // now: the cycle would hop to a session that is not there, and the line
        // printed on the way out would name it too.
        // The history it seeded goes with it, or the next reattach puts it in
        // the scrollback a second time.
        if let Some(name) = renamed {
            let was = where_;
            where_ = qualified(&target.host, &name);
            if seeded.remove(&was) {
                seeded.insert(where_.clone());
            }
            if let Some(known) = &known {
                snapshot.renamed_member(&known.member, &name);
            } else {
                snapshot.renamed(&target, &name);
            }
            cycle.renamed(&name);
            if let Some(known) = &mut known {
                known.renamed(&target, &name);
            }
            cancel_listing(&mut listing).await;
        }
        match outcome {
            // Everything the popup chose, in its own row ids. The lookups are
            // here because this is the half that knows what a row means: the
            // popup is handed rows and hands back ids, and never learns what a
            // host, a group or a pid is.
            Outcome::Chose(chose) => {
                hopped = false;
                // Back to the list, because that is where the gesture was made
                // and none of these is a gesture that goes anywhere: grouping a
                // session, naming one, narrowing to a group all leave you
                // exactly where you were, and dropping into the session
                // afterwards throws away the place you were working from. Enter
                // on a session is the one that means go, and it is the one that
                // sets this back to `Focus` below.
                mode = Mode::Control;
                let subject = match &chose {
                    Chose::Go(row)
                    | Chose::Move { session: row, .. }
                    | Chose::NewGroup { session: row, .. }
                    | Chose::Named { session: row, .. } => Some(*row),
                    Chose::Focus(_) | Chose::On(_) => None,
                };
                // A refresh can mark the row unavailable after the key chose
                // it but before the popup painted the answer.
                if subject.is_some_and(|row| !listed.available(row)) {
                    notice = Some("that session is offline".to_string());
                    listing = Some(spawn_listing(socket));
                    continue;
                }
                match chose {
                    // A hop like any other, so the way back is recorded the
                    // same way and `Motion::Last` reaches it.
                    Chose::Go(row) => {
                        // The one that goes somewhere, and so the one that
                        // leaves you in the session rather than over it.
                        mode = Mode::Focus;
                        if let Some(there) = listed.at.get(row)
                            && there != cycle.current()
                        {
                            cycle.moved_to(there.clone());
                            hopped = true;
                        }
                    }
                    // Narrowing. You move only when the session you are in is
                    // not in the group you just chose, since widening or
                    // choosing the group you are already in should leave you
                    // where you are.
                    Chose::Focus(row) => {
                        let Some(group) = listed.groups.get(row).cloned() else {
                            notice = Some("that group is no longer listed".to_string());
                            continue;
                        };
                        cycle.focus(group);
                        if let Some(next) = cycle.step(Motion::Next)
                            && !cycle.holds(cycle.current())
                        {
                            cycle.moved_to(next);
                            hopped = true;
                        }
                    }
                    // A session on the machine that was picked, and straight
                    // into it. The listing is asked for again rather than
                    // corrected here: the node picked the name, and what the
                    // switch keys walk is what the machines say they are
                    // running.
                    Chose::On(row) => match listed.hosts.get(row).cloned() {
                        // The host list shrank between the listing this id was
                        // drawn from and the Enter, which is the only way a row
                        // has no machine behind it. An ssh to nowhere says less
                        // than staying where you are and saying so.
                        None => notice = Some("that machine is no longer listed".to_string()),
                        Some(host) => match interacting(start_on(socket, &host, None)).await {
                            Ok(name) => {
                                // The one that goes somewhere, so it leaves you
                                // in the session rather than over it, the way
                                // Enter on a session row does. A hop, and the
                                // narrowing that has to go with it: what has
                                // just been started is in no group.
                                // `Cycle::started` is both, and says why there
                                // rather than here.
                                cycle.started(Located::new(&host, &name));
                                hopped = true;
                                mode = Mode::Focus;
                            }
                            // Stay where you were and say so on the row. A key
                            // that does nothing and says nothing reads as a
                            // broken client, and the machine is worth naming:
                            // the one that would not answer is usually not the
                            // one you are sitting on.
                            Err(e) => {
                                debug!("could not start a session on {host}: {e:#}");
                                notice = Some(format!("could not start a session on {host}"));
                            }
                        },
                    },
                    // A local file write and a redraw: nothing detaches for
                    // this, which is why `m` acts on the highlighted row rather
                    // than the session you are attached to.
                    // `n` in the same list is the same act with a group that
                    // does not exist yet.
                    regrouping @ (Chose::Move { .. } | Chose::NewGroup { .. }) => {
                        let (session, group) = match regrouping {
                            Chose::Move { session, group } => {
                                (session, listed.groups.get(group).cloned())
                            }
                            Chose::NewGroup { session, name } => (session, Some(Some(name))),
                            _ => unreachable!("only a regrouping reaches this arm"),
                        };
                        let Some(group) = group else {
                            notice = Some("that group is no longer listed".to_string());
                            continue;
                        };
                        notice = regroup(socket, &listed, session, group, &mut groups)
                            .await
                            .err()
                            .map(|e| format!("{e:#}"));
                        cycle.refresh(snapshot.entries(&groups));
                    }
                    // The row may not be the session at the other end of the
                    // attach stream, and `tag::RENAME` renames that one by
                    // design, so this goes over a connection to its machine.
                    Chose::Named { session, to } => {
                        let at = listed.at.get(session);
                        match rename_at(socket, at, &to).await {
                            Ok(name) => {
                                let was_current = at.is_some_and(|at| at == cycle.current());
                                if let Some(at) = at {
                                    snapshot.renamed(at, &name);
                                    cycle.renamed_at(at, &name);
                                    if let Some(known) = &mut known {
                                        known.renamed(at, &name);
                                    }
                                }
                                if let Some(at) = at
                                    && seeded.remove(&qualified(&at.host, &at.session))
                                {
                                    seeded.insert(qualified(&at.host, &name));
                                }
                                if let Some(at) = at
                                    && was_current
                                {
                                    where_ = qualified(&at.host, &name);
                                }
                            }
                            Err(e) => notice = Some(format!("{e:#}")),
                        }
                    }
                }
                // Whatever it was, what the machines are running may have moved
                // under it, so the next attachment needs fresh rows.
                listing = Some(spawn_listing(socket));
            }
            Outcome::New => {
                hopped = false;
                mode = Mode::Control;
                // A rename during this attach has already updated the cycle.
                let at = cycle.current().clone();
                match interacting(start_beside(socket, &at)).await {
                    Ok(name) => {
                        cycle.started(Located::new(&at.host, &name));
                        hopped = true;
                        mode = Mode::Focus;
                    }
                    Err(e) => notice = Some(format!("could not start a session: {e:#}")),
                }
                listing = Some(spawn_listing(socket));
            }
            Outcome::Switch(motion) => {
                take_listing(&mut listing, &mut cycle, &mut snapshot, &mut groups, false).await;
                hopped = false;
                if let Some(next) = cycle.step(motion) {
                    cycle.moved_to(next);
                    hopped = true;
                }
                // Ask again without waiting: a machine that added a session
                // since our last answer must still become reachable by a key.
                if listing.is_none() {
                    listing = Some(spawn_listing(socket));
                }
                // Nowhere to go is not a reason to drop back to focus: the next
                // key carries on from wherever this one left you.
                mode = Mode::Control;
            }
            // A session this run put you in has ended, so the run has not:
            // back to the one you came from, the way a hop goes. What ended
            // is said on the row you land on, because the line that says it
            // on the way out is only printed by a run that is over, and a
            // screen that changes under somebody with nothing said about it
            // reads as a client that lost its place.
            Outcome::Exited(code) => {
                snapshot.forget(cycle.current());
                // The name and not the whole target, because the row it goes
                // on is narrow and drops a notice that does not fit outright,
                // and because the machine is the one named two columns to its
                // right in all but the rarest fall-back.
                let name = cycle.current().session.clone();
                match cycle.fall_back() {
                    Some(_) => {
                        hopped = false;
                        listing = Some(spawn_listing(socket));
                        notice = Some(format!("{name} exited with status {code}"));
                        ended = Some((Outcome::Exited(code), where_));
                        mode = Mode::Focus;
                    }
                    // Nowhere to fall back to, so this is the session somebody
                    // named on the command line: the command is over and the
                    // status it ended with is the answer.
                    None => break (Outcome::Exited(code), where_),
                }
            }
            // The whole point of the project is that the session is still
            // running on a machine that never noticed you left, so a wifi hop
            // or a closed lid is not a reason to put somebody back at their
            // shell. The screen stays exactly as the session painted it and
            // the mark row says what is happening.
            Outcome::Disconnected => {
                if listing.is_none() {
                    listing = Some(spawn_listing(socket));
                }
                match wait_to_reconnect(&mut held, &where_, &mut lost, &mut mode).await {
                    Wait::Retry => continue,
                    Wait::GiveUp => break (Outcome::Disconnected, where_),
                }
            }
            outcome => break (outcome, where_),
        }
    };
    cancel_listing(&mut listing).await;
    // Before the message, which belongs on the screen the shell gets back.
    held.release().await;
    drop(held);

    match outcome {
        Outcome::Detached => {
            println!("[detached from {where_}]  {}", way_back(&where_, watching));
            Ok(OK)
        }
        Outcome::Exited(code) => {
            println!("[{where_} exited with status {code}]");
            Ok(u8::try_from(code).unwrap_or(1))
        }
        Outcome::Disconnected => {
            println!("[disconnected from {where_}]");
            Ok(FAILED)
        }
        Outcome::Switch(_) | Outcome::Chose(_) | Outcome::New => {
            unreachable!("switches never leave the loop above")
        }
    }
}

/// How to get back to the session just left, beside the line saying it has been.
///
/// A detach is the one moment somebody is certainly reading, and the name they
/// want is on the screen in front of them: without this the way back is `mm ls`
/// and a second look at a name they had a moment ago. Spelled with
/// [`qualified`], so it is what they would type rather than what the code calls
/// it: a session on this machine is its bare name.
///
/// The short alias, because this is a reminder and not documentation. And `v`
/// for a run that was a view: `mm a` would put a keyboard into a session
/// somebody else is working in, which is the one thing whoever typed `mm view`
/// did not ask for.
fn way_back(where_: &str, watching: bool) -> String {
    let verb = if watching { "v" } else { "a" };
    style::faint(&format!("`mm {verb} {where_}` to come back"))
}

/// Count one failed attempt at a lost session and sit out the delay it earns.
///
/// There is no attempt that ends the waiting: [`Wait::GiveUp`] comes from
/// somebody pressing something, which is the only thing that should end it.
/// The session is still running on a machine that never noticed anybody left.
///
/// The keyboard goes back to the session's here, and this is the only place
/// that does it. Control mode belongs to the hop that turned it on, which is
/// why it survives a reattach at all: what follows a hop is usually another
/// one. A wait is not a hop. Nobody pressed anything, the row has been saying
/// the connection went rather than what the keys do, and somebody coming back
/// to it presses the mode key meaning to *enter* control mode; pressed while
/// it is already on, that key leaves it, and the `tab` after it lands in their
/// shell. Left set, the mode outlived every hop for the rest of the run,
/// because nothing else here ever turned it off again.
async fn wait_to_reconnect(
    held: &mut attach::Held,
    where_: &str,
    lost: &mut Option<Lost>,
    mode: &mut Mode,
) -> Wait {
    *mode = Mode::Focus;
    let lost = lost.get_or_insert_with(Lost::new);
    lost.attempts = lost.attempts.saturating_add(1);
    attach::waiting(
        held,
        where_,
        attach::reconnect_after(lost.attempts),
        lost.since.elapsed(),
    )
    .await
}

/// The identity last listed for the current session, retained while offline.
struct KnownSession {
    at: Located,
    member: Member,
}

impl KnownSession {
    fn of(snapshot: &Snapshot, at: &Located) -> Option<Self> {
        Some(Self {
            at: at.clone(),
            member: Member::of(&at.host, snapshot.info(at)?),
        })
    }

    fn renamed(&mut self, at: &Located, name: &str) {
        if self.at == *at {
            self.at.session = name.to_string();
        }
    }

    fn recover(
        &mut self,
        snapshot: &Snapshot,
        cycle: &mut Cycle,
        seeded: &mut HashSet<String>,
    ) -> bool {
        if self.at != *cycle.current() {
            return false;
        }
        let Some(now) = snapshot.locate(&self.member) else {
            return false;
        };
        if now == self.at {
            return false;
        }
        if seeded.remove(&qualified(&self.at.host, &self.at.session)) {
            seeded.insert(qualified(&now.host, &now.session));
        }
        cycle.renamed_at(&self.at, &now.session);
        self.at = now;
        true
    }
}

/// A connection that has gone, for as long as it is gone.
struct Lost {
    /// When it went, which is what the mark row counts from: a screen somebody
    /// walks up to says how long it has been sitting there, and after the first
    /// few tries the delay says nothing about that at all.
    since: Instant,
    /// Attempts to get back that have failed, which is what the delay grows
    /// with.
    attempts: u32,
}

impl Lost {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            attempts: 0,
        }
    }
}

/// Why a session could not be attached to.
///
/// Which half of the trip failed, and the two mean opposite things: a machine
/// that never answered is one to wait for, while a node that answered and has
/// no such session is a listing that went stale under the switch keys.
enum Missed {
    Unreachable(anyhow::Error),
    Gone(anyhow::Error),
}

impl From<Missed> for anyhow::Error {
    fn from(missed: Missed) -> Self {
        match missed {
            Missed::Unreachable(e) | Missed::Gone(e) => e,
        }
    }
}

impl Display for Missed {
    /// For the log line, which wants the reason and not the reading of it.
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Missed::Unreachable(e) | Missed::Gone(e) => write!(f, "{e:#}"),
        }
    }
}

/// How long one attempt at getting back is given before it counts as a
/// failure.
///
/// There has to be a number here, because the one thing an attempt cannot be
/// given is forever. ssh's own `ConnectTimeout` is about reaching a machine,
/// and the way this hangs is a step further in: `ControlMaster=auto` puts every
/// command on one shared connection, and connecting to a control socket has no
/// deadline of any kind. A master whose network went takes everything
/// multiplexed onto it with it, which is a whole terminal sitting on
/// `reconnecting` with a row that has stopped counting and a Ctrl-C nobody is
/// reading, until the master times out on its own schedule or somebody works
/// out what a control socket is. Two clients on one master hang and then come
/// back together, which is the shape of it from outside.
///
/// Ten seconds because it is the wrong side of every honest reattach and the
/// right side of nobody's patience: a handshake onto a machine that is there
/// takes under a second, and a retry costs a line on the row rather than
/// anything real.
const REACH_FOR: Duration = Duration::from_secs(10);

/// One attempt at reaching a session, with a deadline on it once this run has
/// been attached to something.
///
/// Not before that. The first attach of a run is a command somebody typed, and
/// it is allowed to take as long as it takes: a cold ssh, a node being started,
/// an install being offered and answered. What is bounded is a reconnect, which
/// nobody asked for and which has somewhere to go when it fails, namely round
/// the loop to a row that counts down and reads the keyboard.
///
/// Dropping the attempt is what stops it: `ssh::spawn` sets `kill_on_drop`, so
/// the ssh that was hanging goes with the future rather than being left to
/// hold the master open behind us.
async fn reaching(
    attempt: impl Future<Output = Result<Attached, Missed>>,
    bounded: bool,
) -> Result<Attached, Missed> {
    if !bounded {
        return attempt.await;
    }
    match tokio::time::timeout(REACH_FOR, attempt).await {
        Ok(reached) => reached,
        Err(_) => Err(Missed::Unreachable(anyhow!(
            "no answer in {}s",
            REACH_FOR.as_secs()
        ))),
    }
}

/// One popup action against a host, with the same deadline a
/// reconnect gets. No keyboard is read while a popup action is pending.
///
/// The list the new key opens offers every machine you watch, whether or not it
/// answered the last listing, so the row under the highlight may well be one
/// marked `no answer`: pressing Enter on one is an ordinary thing to do. And by
/// then the popup has been torn down and the attached client has returned, so
/// the terminal is in raw mode showing the session's last paint with *nothing
/// reading the keyboard* — worse than the wait a dropped connection gets, which
/// at least counts down and can be left. `ControlMaster` is enough on its own:
/// connecting to a wedged control socket has no deadline of any kind.
///
/// Dropping the attempt is what ends it, `ssh::spawn` setting `kill_on_drop`.
async fn interacting<T>(attempt: impl Future<Output = Result<T>>) -> Result<T> {
    match tokio::time::timeout(REACH_FOR, attempt).await {
        Ok(started) => started,
        Err(_) => bail!("no answer in {}s", REACH_FOR.as_secs()),
    }
}

/// Reuse a recently parked transport, falling back to a fresh one if it died.
/// A refusal from a live node is still a missing session, never a reconnect.
async fn attach_using(
    socket: &Path,
    target: &Located,
    history: u32,
    watching: bool,
    connections: &mut Connections,
) -> Result<Attached, Missed> {
    if let Some(connection) = connections.take(&target.host) {
        match tokio::time::timeout(
            Duration::from_secs(2),
            connection.attach(&target.session, attach::session_size(), history, watching),
        )
        .await
        {
            Ok(Ok(session)) => return Ok(session),
            Ok(Err(e)) if !Gone::marked(&e) => return Err(Missed::Gone(e)),
            _ => debug!("parked connection stopped answering; opening a fresh stream"),
        }
    }
    attach_to(socket, target, history, watching).await
}

/// Open a stream to wherever a session is, and attach to it.
async fn attach_to(
    socket: &Path,
    target: &Located,
    history: u32,
    watching: bool,
) -> Result<Attached, Missed> {
    let stream = open_hushed(socket, &target.host)
        .await
        .map_err(Missed::Unreachable)?;
    stream
        .attach(&target.session, attach::session_size(), history, watching)
        .await
        .map_err(|error| {
            if Gone::marked(&error) {
                Missed::Unreachable(error)
            } else {
                Missed::Gone(error)
            }
        })
}

/// Start a session on the machine picked from the list, for the control key
/// that asks for one.
///
/// A node is started where there is none, the way `mm new` does. The list
/// offers every machine you watch and not only the ones this run has been to,
/// so the machine under the highlight may be one nothing has reached yet: over
/// ssh the agent starts a node on demand, and here it is started by hand.
///
/// What it will not do is install anything. A key pressed inside a session, on
/// a terminal in raw mode with somebody's screen painted over it, is no place
/// to be asking for consent to put a binary on a box, so the consent callback
/// is `None` the way the daemon's and tab completion's are: a machine with no
/// `mm` on it is an error, and the mark row says so. Hushed for the same
/// reason, ssh's account of a machine it could not reach belonging in that
/// error rather than on the painted screen.
async fn start_on(socket: &Path, host: &str, cwd: Option<String>) -> Result<String> {
    let spec = SpawnSpec {
        // The node's counter names it, the way it does for any spawn without
        // one: there is no prompt here and nobody typed anything.
        name: None,
        // The login shell.
        command: Vec::new(),
        // An explicit directory came from this host's snapshot. Otherwise use
        // the caller's directory only locally, as `mm new` does.
        cwd: cwd.or_else(|| is_this_machine(host).then(current_dir).flatten()),
        size: attach::session_size(),
        // A login shell, which the node names for itself.
        label: None,
    };
    let how = Reach {
        start: true,
        hushed: true,
        install: false,
    };
    spawn(reach(socket, host, how).await?, spec).await
}

/// Read the live directory from the same host that will start the new shell.
/// Missing information is an error, never permission to start in another place.
async fn start_beside(socket: &Path, at: &Located) -> Result<String> {
    let Response::Snapshot(sessions) = open_hushed(socket, &at.host)
        .await?
        .call(&Request::Snapshot)
        .await?
    else {
        bail!("unexpected response to snapshot");
    };
    let session = sessions
        .into_iter()
        .find(|session| session.name == at.session)
        .ok_or_else(|| anyhow!("session {} is no longer running", at.session))?;
    let cwd = session
        .cwd
        .ok_or_else(|| anyhow!("this host cannot read the current session's directory"))?;
    start_on(socket, &at.host, Some(cwd)).await
}

/// Put the session on `row` in a group, or take it out of one.
///
/// The pid and the start time membership is keyed on come from the snapshot the
/// popup was drawn from, and that snapshot can be a few seconds old. A fresh
/// listing from the machine holding it refreshes them first, because a
/// stale pid puts the wrong session in a group, which is worse than a round
/// trip on a key nobody presses in a hurry.
async fn regroup(
    socket: &Path,
    listed: &Listed,
    row: usize,
    group: Option<String>,
    groups: &mut Groups,
) -> Result<()> {
    interacting(async {
        let at = listed
            .at
            .get(row)
            .ok_or_else(|| anyhow!("that session is no longer listed"))?;
        let sessions = popup_sessions(socket, &at.host).await?;
        let session = sessions
            .iter()
            .find(|s| s.name == at.session)
            .ok_or_else(|| anyhow!("{} is no longer running", at.session))?;
        groups.update(|groups| groups.set(group.as_deref(), &at.host, session))
    })
    .await
}

/// Rename the session on a popup row, wherever it is running, and answer with
/// the name that stuck.
///
/// Canonicalise the outgoing name with the node's own rule. Its `Ok` confirms
/// that address immediately, without a second listing that could fail after
/// the session has already moved.
async fn rename_at(socket: &Path, at: Option<&Located>, to: &str) -> Result<String> {
    interacting(async {
        let at = at.ok_or_else(|| anyhow!("that session is no longer listed"))?;
        let name = session_name(to);
        match reach(socket, &at.host, popup_reach())
            .await?
            .call(&Request::Rename {
                name: at.session.clone(),
                to: name.clone(),
            })
            .await?
        {
            Response::Ok => Ok(name),
            other => bail!("unexpected response to rename: {other:?}"),
        }
    })
    .await
}

fn popup_reach() -> Reach {
    Reach {
        start: false,
        hushed: true,
        install: false,
    }
}

async fn popup_sessions(socket: &Path, host: &str) -> Result<Vec<SessionInfo>> {
    match reach(socket, host, popup_reach())
        .await?
        .call(&Request::List)
        .await?
    {
        Response::Sessions(sessions) => Ok(sessions),
        other => bail!("unexpected response to list: {other:?}"),
    }
}

/// Every machine's sessions, with the memberships read before asking, which
/// are all a listing that lands late is old enough to prune.
async fn survey(socket: &Path) -> Result<Snapshot> {
    let before = Groups::load().unwrap_or_default();
    let listing = survey_everywhere(socket).await?;
    Ok(Snapshot {
        answered: listing.answering(),
        sessions: listing.sessions,
        before,
        ..Snapshot::default()
    })
}

/// Ask every machine off to one side, keeping the groups read before asking.
fn spawn_listing(socket: &Path) -> JoinHandle<Snapshot> {
    let socket = socket.to_path_buf();
    tokio::spawn(async move {
        match survey(&socket).await {
            Ok(surveyed) => surveyed,
            Err(e) => {
                debug!("could not list sessions for the switch keys: {e:#}");
                Snapshot::default()
            }
        }
    })
}

async fn cancel_listing(pending: &mut Option<JoinHandle<Snapshot>>) {
    if let Some(task) = pending.take() {
        task.abort();
        if let Err(error) = task.await
            && !error.is_cancelled()
        {
            debug!("the cancelled listing failed: {error}");
        }
    }
}

/// Collect a finished listing without holding up a switch. Only the first
/// attach waits briefly, to learn its initial group.
///
/// What is already known stands rather than being replaced by nothing, and a
/// listing still out there asking is left running rather than thrown away.
///
/// This is also where the group file gets tidied: the listing is the only thing
/// that says which sessions are still running, and only the machines that
/// answered count towards it. The tidying is load-bearing beyond tidiness:
/// `Groups::refresh` reads the file before it writes it, so the copy the entries
/// below are built from is the one on disk rather than the one this run started
/// with. That is what lets `Cycle::refresh` see a session the window next door
/// moved out of the group this run is narrowed to.
async fn take_listing(
    pending: &mut Option<JoinHandle<Snapshot>>,
    cycle: &mut Cycle,
    snapshot: &mut Snapshot,
    groups: &mut Groups,
    wait: bool,
) {
    if !wait && pending.as_ref().is_none_or(|task| !task.is_finished()) {
        return;
    }
    let Some(mut task) = pending.take() else {
        return;
    };
    match tokio::time::timeout(LISTING_WAIT, &mut task).await {
        Ok(Ok(landed)) => absorb(landed, snapshot, cycle, groups),
        Ok(Err(e)) => debug!("the listing task did not finish: {e}"),
        Err(_) => *pending = Some(task),
    }
}

/// What the popup's lister last found: the rows it handed the popup, and the
/// snapshot they were built from.
struct Fresh {
    listed: Listed,
    snapshot: Snapshot,
}

/// Take a listing that landed as the one the switch keys walk.
///
/// The group file is tidied on the way in, which reads it before writing it, so
/// the entries the cycle is rebuilt from use the copy on disk rather than the
/// one this run started with.
fn absorb(landed: Snapshot, snapshot: &mut Snapshot, cycle: &mut Cycle, groups: &mut Groups) {
    let _ = landed.refresh(groups);
    *snapshot = landed.following(snapshot);
    cycle.refresh(snapshot.entries(groups));
}

#[cfg(test)]
mod tests {
    use super::*;
    use manymux::client::Stream;
    use manymux::hosts::LOCAL;
    use manymux::proto::{self, FrameReader, HostedSession, Size, tag};
    use std::time::SystemTime;
    use tokio::io::split;
    use tokio::net::UnixListener;

    fn listed_session(name: &str, pid: u32) -> HostedSession {
        HostedSession {
            host: as_listed(LOCAL).to_string(),
            session: SessionInfo {
                name: name.to_string(),
                title: "shell".into(),
                command: "sh".into(),
                pid,
                size: Size::default(),
                attached: 0,
                idle: 0,
                bells: 0,
                started: SystemTime::UNIX_EPOCH,
                node: String::new(),
            },
        }
    }

    #[tokio::test]
    async fn a_rename_with_a_lost_reply_is_recovered_by_session_identity() {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mm-rename-lost-reply-{suffix}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut read = FrameReader::new(stream);
            let frame = read.next().await.unwrap().unwrap();
            let Request::Rename { to, .. } = proto::decode(&frame.body).unwrap() else {
                panic!("the popup must request a rename");
            };
            to
        });
        let before = Snapshot {
            sessions: vec![listed_session("build", 7)],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "build");
        let mut known = KnownSession::of(&before, &current).unwrap();
        assert!(rename_at(&path, Some(&current), "nightly").await.is_err());
        let name = host.await.unwrap();
        std::fs::remove_file(path).unwrap();
        let landed = Snapshot {
            sessions: vec![listed_session(&name, 7)],
            ..Snapshot::default()
        };
        let mut cycle = Cycle::new(current.clone());
        let mut seeded = HashSet::from([qualified(&current.host, &current.session)]);
        assert!(known.recover(&landed, &mut cycle, &mut seeded));
        assert_eq!(cycle.current().session, "nightly");
        assert_eq!(seeded, HashSet::from([qualified(&current.host, "nightly")]));
    }

    #[test]
    fn reconnects_do_not_guess_a_renamed_session_from_replacement_or_cached_rows() {
        let before = Snapshot {
            sessions: vec![listed_session("build", 7)],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "build");
        let mut known = KnownSession::of(&before, &current).unwrap();
        let mut cycle = Cycle::new(current.clone());
        let mut seeded = HashSet::new();
        let mut replacement = listed_session("nightly", 7);
        replacement.session.started += Duration::from_secs(1);
        let landed = Snapshot {
            sessions: vec![replacement, listed_session("other", 8)],
            ..Snapshot::default()
        };
        assert!(!known.recover(&landed, &mut cycle, &mut seeded));
        let cached = Snapshot {
            cached: vec![listed_session("nightly", 7)],
            ..Snapshot::default()
        };
        assert!(!known.recover(&cached, &mut cycle, &mut seeded));
        assert_eq!(cycle.current(), &current);
    }

    #[tokio::test]
    async fn a_confirmed_name_discards_a_pending_older_listing() {
        let before = Snapshot {
            sessions: vec![listed_session("build", 7)],
            ..Snapshot::default()
        };
        let current = Located::new(as_listed(LOCAL), "build");
        let mut known = KnownSession::of(&before, &current).unwrap();
        let mut cycle = Cycle::new(current.clone());
        let mut snapshot = before.clone();
        let mut pending = Some(tokio::spawn(async move { before }));
        let mut seeded = HashSet::new();

        snapshot.renamed_member(&known.member, "newest");
        cycle.renamed("newest");
        known.renamed(&current, "newest");
        cancel_listing(&mut pending).await;
        assert!(pending.is_none());
        assert!(!known.recover(&snapshot, &mut cycle, &mut seeded));
        assert_eq!(cycle.current().session, "newest");
    }

    #[tokio::test]
    async fn a_switch_does_not_wait_for_a_pending_listing() {
        let task = tokio::spawn(async { std::future::pending::<Snapshot>().await });
        let mut pending = Some(task);
        let mut cycle = Cycle::new(Located::new("gpu-box", "build"));
        let mut snapshot = Snapshot::default();
        let mut groups = Groups::default();
        tokio::time::timeout(
            Duration::from_millis(20),
            take_listing(&mut pending, &mut cycle, &mut snapshot, &mut groups, false),
        )
        .await
        .expect("a switch must not wait on a host listing");
        let task = pending
            .take()
            .expect("the listing stays available for the popup");
        assert!(!task.is_finished());
        task.abort();
    }

    #[tokio::test]
    async fn a_connection_lost_during_attach_does_not_forget_the_session() {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mm-handshake-{suffix}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut read = FrameReader::new(stream);
            let frame = read.next().await.unwrap().unwrap();
            assert!(matches!(
                proto::decode::<Request>(&frame.body).unwrap(),
                Request::Attach { .. }
            ));
        });
        let result = attach_to(&path, &Located::new(LOCAL, "build"), 0, false).await;
        server.await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert!(matches!(result, Err(Missed::Unreachable(_))));
    }

    #[tokio::test]
    async fn a_broken_cached_transport_opens_a_fresh_connection() {
        let (client, server) = tokio::io::duplex(4096);
        let (read, mut write) = split(server);
        let stale = tokio::spawn(async move {
            let mut read = FrameReader::new(read);
            read.next().await.unwrap().unwrap();
            manymux::proto::write_msg(
                &mut write,
                tag::RESPONSE,
                &Response::Attached {
                    size: Size::new(80, 24),
                    paste: true,
                    scroll: true,
                    rename: true,
                    events: true,
                    read_only: true,
                    reuse: true,
                },
            )
            .await
            .unwrap();
        });
        let (read, write) = split(client);
        let attached = Stream::from_halves(Box::new(read), Box::new(write))
            .attach("build", Size::new(80, 24), 0, false)
            .await
            .unwrap();
        stale.await.unwrap();
        let mut connections = Connections::default();
        connections.insert(LOCAL.into(), attached.split().into_connection());

        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mm-reuse-{suffix}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let fresh = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = split(stream);
            let mut read = FrameReader::new(read);
            let frame = read.next().await.unwrap().unwrap();
            assert!(matches!(proto::decode::<Request>(&frame.body).unwrap(),
                Request::Attach { name, .. } if name == "test"));
            manymux::proto::write_msg(
                &mut write,
                tag::RESPONSE,
                &Response::Attached {
                    size: Size::new(90, 30),
                    paste: true,
                    scroll: true,
                    rename: true,
                    events: true,
                    read_only: true,
                    reuse: true,
                },
            )
            .await
            .unwrap();
        });
        let attached = attach_using(
            &path,
            &Located::new(LOCAL, "test"),
            0,
            false,
            &mut connections,
        )
        .await
        .unwrap_or_else(|_| panic!("a failed cached transport should open a fresh stream"));
        assert_eq!(attached.size, Size::new(90, 30));
        fresh.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    /// An attempt that never answers, which is what a client multiplexed onto
    /// a dead ssh master is: nothing fails, nothing times out, and the row
    /// stops counting because the loop never comes back round to it.
    async fn never() -> Result<Attached, Missed> {
        std::future::pending().await
    }

    #[tokio::test(start_paused = true)]
    async fn a_reconnect_that_never_answers_is_a_failed_attempt() {
        // tokio's clock, not the wall's: this test spends no real time at all.
        let at = tokio::time::Instant::now();
        let missed = reaching(never(), true).await;
        assert!(matches!(missed, Err(Missed::Unreachable(_))));
        assert!(at.elapsed() >= REACH_FOR, "it waited {:?}", at.elapsed());
    }

    #[tokio::test(start_paused = true)]
    async fn a_popup_rename_cannot_wait_forever_for_the_host() {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mm-rename-deadline-{suffix}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut read = FrameReader::new(stream);
            let request = read.next().await.unwrap().unwrap();
            assert!(matches!(
                proto::decode::<Request>(&request.body).unwrap(),
                Request::Rename { .. }
            ));
            std::future::pending::<()>().await;
        });
        let at = Located::new(LOCAL, "build");
        let answer = tokio::time::timeout(REACH_FOR * 2, rename_at(&path, Some(&at), "test")).await;
        host.abort();
        std::fs::remove_file(path).unwrap();
        assert!(
            answer
                .expect("the rename must return before the outer deadline")
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_confirmed_popup_rename_survives_the_host_disconnecting() {
        let suffix = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("mm-rename-confirmed-{suffix}.sock"));
        let listener = UnixListener::bind(&path).unwrap();
        let host = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = split(stream);
            let mut read = FrameReader::new(read);
            let frame = read.next().await.unwrap().unwrap();
            let Request::Rename { to, .. } = proto::decode(&frame.body).unwrap() else {
                panic!("the popup must request a rename");
            };
            proto::write_msg(&mut write, tag::RESPONSE, &Response::Ok)
                .await
                .unwrap();
            to
        });
        let at = Located::new(LOCAL, "build");
        let answer = rename_at(&path, Some(&at), "nightly build/").await;
        let requested = host.await.unwrap();
        std::fs::remove_file(path).unwrap();
        assert_eq!(answer.unwrap(), "nightly-build");
        assert_eq!(requested, "nightly-build");
    }

    /// A session started on a machine that never answers must not take the
    /// terminal with it. The list offers every machine you watch, so a row
    /// marked `no answer` is one you would press Enter on, and the attempt
    /// reads no keyboard: the popup is gone by then and the row is not being
    /// written. Held open by a wedged `ControlMaster` it was a terminal in raw
    /// mode with nothing reading it and no way out.
    #[tokio::test(start_paused = true)]
    async fn starting_a_session_on_a_machine_that_never_answers_is_given_up_on() {
        async fn never() -> Result<String> {
            std::future::pending().await
        }
        // tokio's clock, so the ten seconds cost nothing. The outer wait is
        // there to fail rather than hang if the deadline goes.
        let at = tokio::time::Instant::now();
        let ended = tokio::time::timeout(REACH_FOR * 10, interacting(never())).await;
        assert!(
            ended.expect("the attempt was never given up on").is_err(),
            "a machine that said nothing was read as a session started"
        );
        assert!(at.elapsed() >= REACH_FOR, "it waited {:?}", at.elapsed());
    }

    /// The first attach of a run is a command somebody typed: a cold ssh, a
    /// node being started and an install being answered are all allowed to
    /// take longer than a reconnect is.
    #[tokio::test(start_paused = true)]
    async fn the_first_attach_of_a_run_is_given_as_long_as_it_takes() {
        assert!(
            tokio::time::timeout(REACH_FOR * 10, reaching(never(), false))
                .await
                .is_err(),
            "the first attach was cut short"
        );
    }
}
