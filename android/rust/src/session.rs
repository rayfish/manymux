//! One session, attached and kept attached.
//!
//! This is the app's whole relationship with a machine: reach it, attach, paint
//! what comes back, send what is typed, and when the connection goes, wait and
//! take it up again. The waiting is the part worth stating, because it is the
//! reason the project exists: the session is still running on a machine that
//! never noticed the phone left, so a client that reported an error and went
//! back to a list would throw away the one thing it is for. So the screen stays
//! as the session last painted it and only the state changes.
//!
//! Three tasks, and the split between them is not tidiness. The read loop
//! answers the host's liveness probe itself (`proto::SILENT_FOR`, an absolute
//! deadline rather than a timeout), so it may not be behind anything: an app in
//! somebody's pocket draws nothing for hours and must stay attached throughout.
//! The screen is fed on a task of its own for the same reason, since a burst of
//! output is the one thing that could put the answer behind a queue.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manymux::client::attach::reconnect_after;
use manymux::client::{Attached, SessionHalves, SessionWriter, Update};
use manymux::lock::held;
use manymux::proto::Size;
use tokio::sync::{mpsc, watch};

use crate::keys::{Identity, KnownHosts};
use crate::machine::{Connection, Connections, Machine};
use crate::mouse::At;
use crate::screen::{Frame, Screen};
use crate::scroll::{Scrolling, Window};
use crate::ssh::{ask, reach};

/// How many chunks of output may be waiting to be painted.
///
/// Bounded, so a machine printing faster than the phone paints stops being read
/// and the ssh window closes behind it. The screen collapses whatever arrives
/// into a grid of a fixed size, so this fills only when the app itself is
/// stopped, which is exactly when it should.
const BACKLOG: usize = 64;

/// Where the client is with this session.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Enum)]
pub enum State {
    /// On the way to it: connecting, climbing the ladder, attaching.
    Reaching,
    Attached,
    /// The connection went. The screen is the one it left behind, which is the
    /// point: nothing about the session has changed and there is nothing for a
    /// clock to decide, so this does not run out.
    Waiting {
        /// How many attempts have failed, which is what sets the delay.
        tries: u32,
    },
    /// The session's own process exited, with its status.
    Ended {
        status: i32,
    },
    /// Detached on purpose.
    Detached,
    /// The first attach did not work. Only the first: after that a failure is
    /// a connection that went rather than a command that did not work, and is
    /// waited out rather than reported.
    Failed {
        why: String,
    },
}

/// What the app asks of a session it is attached to.
enum Say {
    Input(Vec<u8>),
    Resize(Size),
    /// The view moved, so whatever it now wants should be asked for.
    ///
    /// The move itself has already happened: it is arithmetic over a block
    /// that is already here, so it is done under the lock where it was asked
    /// for rather than waited on. What has to reach the host is only the block
    /// the move left the view short of, which may be nothing at all.
    Look,
    Detach,
}

/// How long an attempt to get back to a session may take.
///
/// The waiting between attempts is unbounded on purpose, and reads the
/// keyboard throughout. The *attempt* reads nothing, so one that never returns
/// is a screen frozen on "reconnecting" with nothing counting down and a back
/// button nobody is reading. russh puts no deadline on a connect, and a TCP
/// that reaches something which never sends a banner will sit there for as
/// long as the network lets it.
///
/// It applies to reconnects alone: the first attach of a run is a thing
/// somebody asked for, and may legitimately be a cold ssh, a node starting, or
/// an install being answered.
const REACH_FOR: Duration = Duration::from_secs(10);

/// How long an attach has to last before it counts as having worked.
const STEADY: Duration = Duration::from_secs(30);

/// Why an attach did not happen.
///
/// The two mean opposite things and the distinction is the whole of what
/// decides between waiting and reporting: a machine that never answered is
/// waited for, while a node that answered and has no such session is a session
/// that ended, and reconnecting to it every ten seconds forever is the bug
/// this exists to stop.
///
/// `main.rs` draws the same line and then waits out both, on the grounds that
/// a node restarting is a session that can come back under the same name. This
/// end diverges deliberately: there is no cycle to fall back to and no hop to
/// undo, so waiting means a phone showing a dead screen and a bar that says
/// "reconnecting" forever about something the node has said outright is not
/// there. Reporting it puts somebody back at the list, where they can start
/// another one.
enum Missed {
    Machine(anyhow::Error),
    Gone(String),
}

/// What ended an attach.
enum Ending {
    Exited(i32),
    Detached,
    /// The connection went, which is not an ending at all.
    Lost,
}

/// What a drag turned out to mean.
///
/// Three answers because there are three things the app draws next: nothing at
/// all, a view that is now worth asking about, or a sentence saying the
/// gesture has nowhere to go. The third is the one worth having a value for: a
/// key or a drag that quietly does nothing is the thing this project keeps
/// refusing to ship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Dragged {
    /// The session is reading the mouse, and has been sent the notches.
    Wheeled,
    /// The view moved, so it is worth asking what it looks like now.
    Looked,
    /// Neither is possible here, which is a host too old for the view holding
    /// a session that is not reading the mouse.
    Nowhere,
}

/// A chunk on its way to the screen.
enum Paint {
    /// The screen as the host has it. Everything before it is gone.
    Repaint(Vec<u8>),
    Feed(Vec<u8>),
    Resized(Size),
}

/// An attached session, from the app's side.
///
/// Cheap to hold and safe to call from anywhere: every method either takes the
/// screen's lock for as long as it takes to copy some rows, or drops a message
/// into a queue.
pub struct Session {
    screen: Arc<Mutex<Screen>>,
    /// The view over the host's history, which is a second surface and not a
    /// state of the first: the session goes on printing into the screen while
    /// somebody reads what it printed a minute ago, and coming back costs
    /// nothing because nothing was thrown away.
    scrolling: Arc<Mutex<Scrolling>>,
    /// Whether the host can be scrolled back through at all, as it said when
    /// it answered the attach. A build too old to know the request would skip
    /// it in silence, so a gesture that quietly did nothing is the thing this
    /// exists to stop.
    scrolls: Arc<AtomicBool>,
    say: mpsc::UnboundedSender<Say>,
    state: watch::Receiver<State>,
}

impl Session {
    /// Reach `name` on `machine` and stay attached to it.
    ///
    /// Returns at once: everything happens on tasks, and [`Session::state`]
    /// says how far it has got.
    pub fn open(
        machine: Machine,
        identity: Identity,
        known: KnownHosts,
        connections: Arc<Connections>,
        name: String,
        size: Size,
    ) -> Self {
        let screen = Arc::new(Mutex::new(Screen::at(size)));
        let scrolling = Arc::new(Mutex::new(Scrolling::at(size)));
        let scrolls = Arc::new(AtomicBool::new(false));
        let (say, said) = mpsc::unbounded_channel();
        let (state, watching) = watch::channel(State::Reaching);

        tokio::spawn(keep_attached(
            Attaching {
                machine,
                identity,
                known,
                connections,
                name,
                wanted: size,
            },
            Surfaces {
                screen: Arc::clone(&screen),
                scrolling: Arc::clone(&scrolling),
                scrolls: Arc::clone(&scrolls),
            },
            said,
            state,
        ));

        Self {
            screen,
            scrolling,
            scrolls,
            say,
            state: watching,
        }
    }

    /// The rows that have changed since this was last asked.
    ///
    /// Called once a frame. Never required for anything else to work: a session
    /// nobody is drawing stays attached, which is what an app in the background
    /// is.
    pub fn take_frame(&self) -> Frame {
        held(&self.screen).take_frame()
    }

    /// Keystrokes, or anything else typed at the session.
    pub fn send(&self, bytes: Vec<u8>) {
        let _ = self.say.send(Say::Input(bytes));
    }

    /// The phone's screen is a different shape now.
    pub fn resize(&self, size: Size) {
        let _ = self.say.send(Say::Resize(size));
    }

    /// A drag of whole lines, positive being back towards what came before.
    ///
    /// Where the two things a drag can mean are told apart, and the answer
    /// says which happened so the app can draw what follows from it. A session
    /// reading the mouse gets the notches and nothing else does: two readers
    /// on one wheel is one of them reading input meant for the other, and a
    /// full-screen program draws its own scrolling from exactly these reports.
    /// That is the desktop's rule, which its terminal settles for it; here
    /// there is no terminal, and a drag that opened the history view over a
    /// program on the alternate screen opened it over a buffer with no history
    /// behind it at all.
    ///
    /// The move happens here rather than on the task, because it is arithmetic
    /// over the block already in hand: a drag reports a row at a time and
    /// every one of them would otherwise be a trip through a queue before the
    /// screen it moved could be drawn. What goes to the task is the asking,
    /// and only when the move left the view short of lines.
    pub fn drag(&self, lines: i64, at: At) -> Dragged {
        if let Some(notches) = self.wheel(lines, at) {
            self.send(notches);
            return Dragged::Wheeled;
        }
        if lines > 0 {
            // The host cannot answer for a window, and the session is not
            // reading the mouse either, so there is nothing this can do but
            // say so where the gesture was made.
            if !self.scrolls.load(Ordering::Relaxed) {
                return Dragged::Nowhere;
            }
            held(&self.scrolling).up(lines as u64);
        } else {
            held(&self.scrolling).down(lines.unsigned_abs());
        }
        let _ = self.say.send(Say::Look);
        Dragged::Looked
    }

    /// The drag as wheel reports, where the session asked to be sent them.
    ///
    /// The screen's lock is taken and given back before the view's, which is
    /// the only order these two are ever taken in.
    fn wheel(&self, lines: i64, at: At) -> Option<Vec<u8>> {
        let screen = held(&self.screen);
        if !screen.mouse().wanted() {
            return None;
        }
        let up = lines > 0;
        let notch = screen.mouse().wheel(up, at.col, at.row);
        Some(notch.repeat(lines.unsigned_abs() as usize))
    }

    /// Back to the live screen.
    pub fn close_view(&self) {
        held(&self.scrolling).close();
    }

    /// The view as it should now look, taken the way a frame is.
    pub fn take_window(&self) -> Window {
        held(&self.scrolling).take_window()
    }

    /// Leave, without ending anything.
    pub fn detach(&self) {
        let _ = self.say.send(Say::Detach);
    }

    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }
}

/// What it takes to attach, kept together because a reattach needs all of it
/// again.
///
/// Cloned per attempt so that the attempt can be waited on and given up while
/// the size it was asked for goes on changing under it.
#[derive(Clone)]
struct Attaching {
    machine: Machine,
    identity: Identity,
    known: KnownHosts,
    /// Shared with the app, so a session opened after somebody went back to
    /// the list rides the connection the list was drawn over.
    connections: Arc<Connections>,
    name: String,
    /// The size the phone is asking for, which is not the size the session
    /// settles on: the node takes the smallest of every attached client's.
    wanted: Size,
}

/// What the app draws, and what says whether the second of them is worth
/// drawing at all. Held by the task for as long as the session is, and by the
/// app through [`Session`].
struct Surfaces {
    screen: Arc<Mutex<Screen>>,
    scrolling: Arc<Mutex<Scrolling>>,
    scrolls: Arc<AtomicBool>,
}

/// One attach, and the connection it is riding on.
///
/// The connection is held because dropping the last handle to it closes the
/// ssh session under the channel the attach is on. Shared rather than owned:
/// the pool holds one too, which is what lets the next attach skip the
/// handshake.
struct Riding {
    _connection: Arc<Connection>,
    attached: Attached,
}

/// Reach the session, attach, and keep doing so.
async fn keep_attached(
    mut attaching: Attaching,
    surfaces: Surfaces,
    mut said: mpsc::UnboundedReceiver<Say>,
    state: watch::Sender<State>,
) {
    let mut tries = 0u32;
    let mut ever = false;

    loop {
        let _ = state.send(State::Reaching);
        let reached = reaching(
            attaching.clone(),
            ever,
            &surfaces,
            &mut said,
            &mut attaching.wanted,
        )
        .await;
        match reached {
            // The back button, pressed while it was still trying.
            None => {
                let _ = state.send(State::Detached);
                return;
            }
            Some(Ok(riding)) => {
                ever = true;
                surfaces
                    .scrolls
                    .store(riding.attached.scroll, Ordering::Relaxed);
                // The history has moved under anything the view was showing,
                // and the offsets it holds count back from a newest line that
                // is not the newest line any more. So a reattach is where a
                // view goes, rather than being corrected into one that could
                // never be told from a stale one.
                held(&surfaces.scrolling).close();
                let _ = state.send(State::Attached);
                let began = Instant::now();
                match pump(riding, &surfaces, &mut said, &mut attaching.wanted).await {
                    Ending::Exited(code) => {
                        let _ = state.send(State::Ended { status: code });
                        return;
                    }
                    Ending::Detached => {
                        let _ = state.send(State::Detached);
                        return;
                    }
                    // The count starts again only for an attach that lasted,
                    // or a machine that accepts a connection and hangs up
                    // immediately would be retried every second forever, each
                    // one a full handshake on a phone's battery. The same
                    // reasoning as `peers::STEADY`, and the same shape of
                    // machine behind it.
                    Ending::Lost => {
                        if began.elapsed() >= STEADY {
                            tries = 0;
                        }
                    }
                }
            }
            // The node answered and has no such session, which is an answer
            // rather than a connection that went.
            Some(Err(Missed::Gone(why))) => {
                let _ = state.send(State::Failed { why });
                return;
            }
            Some(Err(Missed::Machine(error))) => {
                // The first attach is a thing somebody asked for, and its
                // failure is an answer: the machine is not reachable. Every
                // failure after it is a connection that went, and the session
                // is still running.
                if !ever {
                    let _ = state.send(State::Failed {
                        why: format!("{error:#}"),
                    });
                    return;
                }
            }
        }

        tries = tries.saturating_add(1);
        let _ = state.send(State::Waiting { tries });
        let waiting = tokio::time::sleep(reconnect_after(tries));
        tokio::pin!(waiting);
        loop {
            tokio::select! {
                _ = &mut waiting => break,
                asked = said.recv() => match asked {
                    // The back button, pressed while the screen says the
                    // connection went. There is nothing to detach from, so it
                    // is this that ends the run.
                    Some(Say::Detach) | None => {
                        let _ = state.send(State::Detached);
                        return;
                    }
                    // Nothing to send it to. Held and delivered later, it would
                    // be a keystroke arriving in somebody's shell minutes after
                    // they pressed it.
                    Some(Say::Input(_)) => {}
                    // Nowhere to ask, and the block already in hand is what
                    // the view has to work with until there is somewhere.
                    Some(Say::Look) => {}
                    Some(Say::Resize(size)) => {
                        attaching.wanted = size;
                        held(&surfaces.scrolling).resize(size);
                    }
                },
            }
        }
    }
}

/// One attempt, which the app can give up on and which gives up on itself.
///
/// `None` is somebody who left while it was out. The keyboard is read
/// throughout for the same reason the waiting reads it: an attempt is not a
/// state anybody should be stuck in, and what is typed at one is dropped
/// rather than queued, or it arrives in a shell minutes after it was pressed.
async fn reaching(
    attaching: Attaching,
    ever: bool,
    surfaces: &Surfaces,
    said: &mut mpsc::UnboundedReceiver<Say>,
    wanted: &mut Size,
) -> Option<Result<Riding, Missed>> {
    // Named before the attempt takes it, so the message a deadline produces
    // can still say which machine it was about.
    let at = attaching.machine.at();
    let attempt = attach(attaching);
    tokio::pin!(attempt);
    let deadline = tokio::time::sleep(if ever { REACH_FOR } else { Duration::MAX });
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            got = &mut attempt => return Some(got),
            // Dropping the attempt is what ends it, which works because
            // russh's connection goes with the future holding it.
            _ = &mut deadline => {
                return Some(Err(Missed::Machine(anyhow::anyhow!(
                    "gave up reaching {} after {}s",
                    at,
                    REACH_FOR.as_secs()
                ))));
            }
            asked = said.recv() => match asked {
                Some(Say::Detach) | None => return None,
                Some(Say::Input(_)) | Some(Say::Look) => {}
                Some(Say::Resize(size)) => {
                    *wanted = size;
                    held(&surfaces.scrolling).resize(size);
                }
            },
        }
    }
}

/// Connect, climb the ladder, and attach.
///
/// What tells a session that ended from a connection that went is the
/// *listing*, not which call failed. Reading the failure of `Stream::attach`
/// as the node's answer, as this once did, was wrong twice over: that call
/// begins with a request, so a radio dropping in the window before the answer
/// arrives came back as a session that had ended, and the run was reported and
/// over while the session sat there on a machine that never noticed.
///
/// The listing costs nothing, being the same round trip that climbs the
/// ladder, and it is unambiguous: a node that answered with the sessions it
/// has and did not name this one has said the session is gone.
async fn attach(attaching: Attaching) -> Result<Riding, Missed> {
    let connection = attaching
        .connections
        .to(&attaching.machine, &attaching.identity, &attaching.known)
        .await
        .map_err(Missed::Machine)?;

    // A pooled connection is one that worked a moment ago, which on a phone is
    // no promise at all: a radio that slept leaves one that looks open and
    // completes nothing. So a failure on it puts it out of the pool rather
    // than leaving the next attempt to fail on the same dead handle.
    let reached = match reach(&*connection).await {
        Ok(reached) => reached,
        Err(error) => {
            attaching.connections.forget().await;
            return Err(Missed::Machine(error));
        }
    };

    if !reached
        .sessions
        .iter()
        .any(|session| session.name == attaching.name)
    {
        return Err(Missed::Gone(format!(
            "there is no session called {} on {} any more",
            attaching.name,
            attaching.machine.at()
        )));
    }

    // A stream of its own, because the listing spent the one it came on: a
    // node answers one request per connection and hangs up.
    let stream = ask(&*connection, reached.program)
        .await
        .map_err(Missed::Machine)?;

    // No history: the phone has no scrollback of its own to put it in, and the
    // node keeps the real one.
    let attached = stream
        .attach(&attaching.name, attaching.wanted, 0, false)
        .await
        .map_err(Missed::Machine)?;
    Ok(Riding {
        _connection: connection,
        attached,
    })
}

/// Ask the host for whatever the view is short of, and answer whether the
/// connection survived it.
///
/// Nothing is asked while the view is closed, or while what it is showing sits
/// inside the block already here, which is what makes a drag through one cost
/// no network at all. The lock is dropped before the write: everything here is
/// arithmetic, and a lock held across an await is the one thing this client
/// may not do.
async fn look(scrolling: &Mutex<Scrolling>, writer: &mut SessionWriter) -> bool {
    let Some(request) = held(scrolling).wanted() else {
        return true;
    };
    writer.view(&request).await.is_ok()
}

/// Read the session until something ends it.
async fn pump(
    riding: Riding,
    surfaces: &Surfaces,
    said: &mut mpsc::UnboundedReceiver<Say>,
    wanted: &mut Size,
) -> Ending {
    let screen = &surfaces.screen;
    let settled = riding.attached.size;
    let SessionHalves {
        mut reader,
        mut writer,
    } = riding.attached.split();

    let (painting, mut to_paint) = mpsc::channel::<Paint>(BACKLOG);
    let painter = {
        let screen = Arc::clone(screen);
        tokio::spawn(async move {
            while let Some(paint) = to_paint.recv().await {
                let mut screen = held(&screen);
                match paint {
                    Paint::Repaint(bytes) => screen.repaint(&bytes),
                    Paint::Feed(bytes) => screen.feed(&bytes),
                    Paint::Resized(size) => screen.resize(size),
                }
            }
        })
    };

    // The size the session settled on, which with a desktop attached may be
    // smaller than what was asked for.
    let _ = painting.send(Paint::Resized(settled)).await;
    held(&surfaces.scrolling).resize(settled);
    // The first output after an attach is the repaint, and only the first.
    let mut repainting = true;

    let ending = loop {
        tokio::select! {
            update = reader.next() => match update {
                Ok(Update::Output(bytes)) => {
                    let paint = if std::mem::take(&mut repainting) {
                        Paint::Repaint(bytes)
                    } else {
                        Paint::Feed(bytes)
                    };
                    if painting.send(paint).await.is_err() {
                        break Ending::Lost;
                    }
                }
                // The screen again, in answer to a resync. A repaint like any
                // other: what was on the screen before it is gone.
                Ok(Update::Screen(bytes)) => {
                    if painting.send(Paint::Repaint(bytes)).await.is_err() {
                        break Ending::Lost;
                    }
                }
                // Answered here and nowhere else. A client that waited for the
                // app to draw before saying it was alive would be detached by
                // the host for being in somebody's pocket.
                // What the session actually became, which is not always what
                // was asked for: the node takes the smallest across every
                // attached client. Reflowing this end's copy to the size that
                // was asked for, as it did before the node said, paints a
                // screen the session never had and the two scroll at different
                // rows from then on.
                Ok(Update::Resized(size)) => {
                    held(&surfaces.scrolling).resize(size);
                    if !look(&surfaces.scrolling, &mut writer).await {
                        break Ending::Lost;
                    }
                    if painting.send(Paint::Resized(size)).await.is_err() {
                        break Ending::Lost;
                    }
                }
                Ok(Update::Ping) => {
                    if writer.pong().await.is_err() {
                        break Ending::Lost;
                    }
                }
                // A block of the history, for the view. Taking it can leave
                // the window somewhere the block does not reach, which is
                // ordinary rather than a mistake: a view thrown further back
                // than the buffer goes is clamped by the first answer that
                // says where the end is. So whatever is wanted after taking
                // one is asked for.
                Ok(Update::View(view)) => {
                    held(&surfaces.scrolling).took(view);
                    if !look(&surfaces.scrolling, &mut writer).await {
                        break Ending::Lost;
                    }
                }
                Ok(Update::Exited(code)) => break Ending::Exited(code),
                Ok(Update::Disconnected) => break Ending::Lost,
                // History was not asked for, and a bell in the session next
                // door is a notification this version does not send.
                Ok(_) => {}
                Err(_) => break Ending::Lost,
            },
            asked = said.recv() => match asked {
                Some(Say::Input(bytes)) => {
                    if writer.send_input(&bytes).await.is_err() {
                        break Ending::Lost;
                    }
                }
                Some(Say::Resize(size)) => {
                    *wanted = size;
                    // A window of a different height is one the block in hand
                    // may no longer cover, and the view is showing lines
                    // rather than the session: nothing else is going to redraw
                    // it.
                    held(&surfaces.scrolling).resize(size);
                    if !look(&surfaces.scrolling, &mut writer).await {
                        break Ending::Lost;
                    }
                    // Optimistically, and corrected by `Update::Resized` when
                    // the node says what it took. A node too old to say leaves
                    // this as the only answer there is, which is what happened
                    // before it could.
                    if painting.send(Paint::Resized(size)).await.is_err() {
                        break Ending::Lost;
                    }
                    if writer.resize(size).await.is_err() {
                        break Ending::Lost;
                    }
                    // Telling the node the size redraws nothing: a session that
                    // printed and went quiet has no answer to a SIGWINCH, so
                    // both ends reflow on their own and do it differently, the
                    // node having a scrollback to pull lines back out of and
                    // this end having none. Two screens a few rows out of step
                    // is every cursor-addressed write landing on the wrong
                    // line, and nothing repairs it short of reattaching. So the
                    // screen is asked for, and the answer is a repaint.
                    if writer.resync().await.is_err() {
                        break Ending::Lost;
                    }
                }
                // Whatever the view is short of after a move that has already
                // happened. Nothing at all while it is closed, or while what
                // it is showing is inside the block already here, which is
                // what makes a drag through one cost no network at all.
                Some(Say::Look) => {
                    if !look(&surfaces.scrolling, &mut writer).await {
                        break Ending::Lost;
                    }
                }
                Some(Say::Detach) => {
                    let _ = writer.detach().await;
                    break Ending::Detached;
                }
                // Every handle is gone, so there is nobody left to be attached
                // on behalf of.
                None => break Ending::Detached,
            },
        }
    };

    drop(painting);
    let _ = painter.await;
    ending
}
