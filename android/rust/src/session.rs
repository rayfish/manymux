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

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use manymux::client::attach::reconnect_after;
use manymux::client::{Attached, SessionHalves, Update};
use manymux::lock::held;
use manymux::proto::Size;
use tokio::sync::{mpsc, watch};

use crate::keys::{Identity, KnownHosts};
use crate::machine::{Connection, Machine};
use crate::screen::{Frame, Screen};
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
        name: String,
        size: Size,
    ) -> Self {
        let screen = Arc::new(Mutex::new(Screen::at(size)));
        let (say, said) = mpsc::unbounded_channel();
        let (state, watching) = watch::channel(State::Reaching);

        tokio::spawn(keep_attached(
            Attaching {
                machine,
                identity,
                known,
                name,
                wanted: size,
            },
            Arc::clone(&screen),
            said,
            state,
        ));

        Self {
            screen,
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
    name: String,
    /// The size the phone is asking for, which is not the size the session
    /// settles on: the node takes the smallest of every attached client's.
    wanted: Size,
}

/// One attach, and the connection it is riding on.
///
/// The connection is held because dropping it closes the ssh session under the
/// channel the attach is on.
struct Riding {
    _connection: Connection,
    attached: Attached,
}

/// Reach the session, attach, and keep doing so.
async fn keep_attached(
    mut attaching: Attaching,
    screen: Arc<Mutex<Screen>>,
    mut said: mpsc::UnboundedReceiver<Say>,
    state: watch::Sender<State>,
) {
    let mut tries = 0u32;
    let mut ever = false;

    loop {
        let _ = state.send(State::Reaching);
        let reached = reaching(attaching.clone(), ever, &mut said, &mut attaching.wanted).await;
        match reached {
            // The back button, pressed while it was still trying.
            None => {
                let _ = state.send(State::Detached);
                return;
            }
            Some(Ok(riding)) => {
                ever = true;
                let _ = state.send(State::Attached);
                let began = Instant::now();
                match pump(riding, &screen, &mut said, &mut attaching.wanted).await {
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
                    Some(Say::Resize(size)) => attaching.wanted = size,
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
                Some(Say::Input(_)) => {}
                Some(Say::Resize(size)) => *wanted = size,
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
    let connection = Connection::open(&attaching.machine, &attaching.identity, &attaching.known)
        .await
        .map_err(Missed::Machine)?;
    let reached = reach(&connection).await.map_err(Missed::Machine)?;

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
    let stream = ask(&connection, reached.program)
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

/// Read the session until something ends it.
async fn pump(
    riding: Riding,
    screen: &Arc<Mutex<Screen>>,
    said: &mut mpsc::UnboundedReceiver<Say>,
    wanted: &mut Size,
) -> Ending {
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
                    if painting.send(Paint::Resized(size)).await.is_err() {
                        break Ending::Lost;
                    }
                }
                Ok(Update::Ping) => {
                    if writer.pong().await.is_err() {
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
