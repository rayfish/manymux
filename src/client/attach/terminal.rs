//! Driving a real terminal: raw mode, what to write when the terminal changes
//! hands, and the loop that moves bytes between the session and the screen.
//!
//! Everything here is desktop-only. A mobile client drives
//! [`crate::client::Attached`] directly and paints with its own widget, so none
//! of this is in the build it links against.

use std::fmt::Write as _;
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use crossterm::terminal;
use tokio::io::{AsyncWriteExt, Stdout};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use super::keys::wheel_is_ours;
use super::{Action, Chose, Find, KeyFilter, Mode, Outcome, Pick, Rename, Rows, Scroll, Select};
use crate::client::picker::Picker;
use crate::client::screen::ScreenMode;
use crate::client::scroll::{self, Chased, Scrollback};
use crate::client::status::{self, Filter, Popped, Status};
use crate::client::{Attached, SessionHalves, SessionReader, SessionWriter, Update};
use crate::clipboard;
use crate::notify;
use crate::proto::{HostedEvent, Renamed, Size};
use crate::settings::{Mouse, Screen, Settings};

/// Sent before attaching.
///
/// The title is pushed because detaching should give you back the tab name
/// you had, not leave it named after a session you left. What the mode adds
/// is the screen, and why it adds what it adds is in
/// [`crate::client::screen`].
fn setup(mode: &dyn ScreenMode) -> String {
    format!("\x1b[22;2t{}", mode.setup()) // push the window title
}

/// Terminal state a full-screen program may have left behind, undone
/// whenever the terminal changes hands, so it is never inherited by whoever
/// gets it next: a shell left with an invisible cursor, a stuck mouse mode
/// or focus reporting still on, and equally the session you hop to.
///
/// The private modes come from the same list a reattach replays: whatever
/// the node switches back on for the session is exactly what has to be
/// switched off again here, or it leaks into what follows.
fn undone() -> String {
    let mut undone = String::new();
    // Both of these home the cursor, so they go first, while the alternate
    // screen is still up and the cursor there is about to be discarded.
    // Afterwards they would drop the shell's prompt in the top-left corner.
    undone.push_str("\x1b[r"); // full-height scrolling region
    undone.push_str("\x1b[?6l"); // absolute cursor addressing, not origin mode

    undone.push_str("\x1b[?25h"); // show the cursor
    undone.push_str("\x1b[0m"); // default attributes
    undone.push_str("\x1b[?7h"); // autowrap on
    undone.push_str("\x1b[4l"); // replace mode, not insert
    undone.push_str("\x1b[?1l\x1b>"); // normal cursor keys and keypad
    undone.push_str(&given_back());
    undone
}

/// Everything the program in the session switched on, switched back off: the
/// private modes a reattach replays, the extended-keys protocols, and the
/// cursor shape.
///
/// Exactly the set [`crate::node::events::Scanner::replay`] answers for, and
/// that pairing is the whole point of it. A replay says what is *on* and has no
/// way to say what was turned off, so it can only be trusted about a terminal
/// that has just been put back to nothing. Both callers need that. A hop hands
/// the terminal to a session that never asked for any of this; a screen the
/// client asked for is answering for a stretch where the session was not being
/// painted at all, and a mode switched off in that stretch has no other way of
/// ever reaching the terminal. That one showed up as a program leaving the
/// terminal reporting key releases at the shell that followed it: the pop was
/// dropped under the popup, and every keystroke after it typed `[103;1:3u`
/// into whatever was running.
///
/// The kitty count is the whole stack, because the pushing was the program's
/// and there is no telling how deep it went; popping past the bottom does
/// nothing. The set that follows is for a program that changed the flags
/// without pushing, which the pops cannot undo.
fn given_back() -> String {
    let mut off = String::new();
    off.push_str("\x1b[0 q"); // the cursor shape this terminal defaults to
    for mode in crate::node::events::REPLAYED_MODES {
        let _ = write!(off, "\x1b[?{mode}l");
    }
    off.push_str("\x1b[<16u\x1b[=0;1u");
    off.push_str("\x1b[>4;0m"); // and xterm's older modifyOtherKeys
    off
}

/// Sent before every attach, because the terminal is changing hands there
/// too: the session you are leaving switched modes on that the one you are
/// arriving at never asked for, and its screen is still up.
///
/// What to do about that screen is the mode's, since only one of the two
/// owns it. `on_alternate` is whether the session being left has the
/// terminal on a full-screen program's own screen.
pub(super) fn takeover(mode: &dyn ScreenMode, on_alternate: bool) -> String {
    format!("{}{}", undone(), mode.takeover(on_alternate))
}

/// Written before a screen the node sent in answer to a resize.
///
/// Nothing in the session redraws for a size it was never told about: a
/// shell that printed and went quiet has nothing to say about the window
/// changing shape, so the node's model is the only place the screen exists
/// at its new size. Painting it needs the same two things a hop needs, and
/// for the same reasons: the erase, because the dump paints from the cursor
/// down to its last line with anything on it and never erases, so the old
/// geometry shows through under and beside it, marks on what used to be the
/// bottom row included; and the home, because that is where the dump starts
/// printing.
///
/// Where a hop resets the terminal first, this cannot: the session is still
/// running and still owns every mode it switched on. So the pen is put back
/// by hand instead, since the erase clears to the current background and a
/// program that left one set would otherwise paint the whole screen its
/// colour.
const REGROWN: &str = concat!(
    "\x1b7",   // save the cursor, and the pen with it
    "\x1b[0m", // default attributes, so the erase clears to the usual background
    "\x1b[H",  // home, since erasing does not move the cursor
    "\x1b[2J", // the screen the old size left behind
    "\x1b8",   // the pen and the cursor back
    "\x1b[H",  // and home again, where the dump starts printing
);

/// Everything [`undone`] undoes, the title given back, and the screen left
/// however the mode leaves it. A detach, in other words, where a hop stops
/// at [`takeover`].
///
/// `on_alternate` is whether the session has the terminal on a full-screen
/// program's own screen, which only the attach loop can see and which only
/// the inline mode has to do anything about.
pub(super) fn reset(mode: &dyn ScreenMode, on_alternate: bool) -> String {
    let mut reset = undone();
    reset.push_str("\x1b[23;2t"); // pop the title pushed on attach
    let _ = write!(reset, "{}", mode.reset(terminal_size(), on_alternate));
    reset
}

/// The terminal, whole.
pub fn terminal_size() -> Size {
    terminal::size()
        .map(|(cols, rows)| Size::new(cols, rows))
        .unwrap_or_default()
        .sane()
}

/// The part of it the session gets, which is everything above the mark.
pub fn session_size() -> Size {
    status::session_size(terminal_size())
}

/// The terminal, held in raw mode on the alternate screen for as long as
/// this lives, and given back whole when it is dropped.
///
/// Held across a run of attaches rather than one, so switching sessions
/// does not flap the alternate screen between every hop. What makes that
/// safe is [`takeover`], written per attach: the screen is one surface, but
/// no session inherits the one before it. Dropping this is what restores
/// the terminal, on the error paths too.
pub struct Held {
    /// The keyboard, owned here rather than by an attach, because it
    /// outlives one. See [`keyboard`].
    keys: mpsc::Receiver<Vec<u8>>,
    screen: Screen,
    /// Whether the session has the terminal on its own alternate screen,
    /// which only the attach loop can see and which both the teardown and
    /// the panic hook need.
    on_alternate: Arc<AtomicBool>,
}

pub fn hold(screen: Screen) -> Result<Held> {
    if !std::io::stdin().is_terminal() {
        bail!("attach needs a terminal on stdin");
    }
    let on_alternate = Arc::new(AtomicBool::new(false));
    // A panic prints to stderr, which while this is held means printing
    // over the session's screen, and on a screen of the client's own the
    // unwind then throws it away. Give the terminal back first, so whatever
    // went wrong is readable on the screen the shell gets back.
    let previous = std::panic::take_hook();
    let flagged = Arc::clone(&on_alternate);
    std::panic::set_hook(Box::new(move |panic| {
        let _ = terminal::disable_raw_mode();
        write_now(&reset(screen.mode(), flagged.load(Ordering::Relaxed)));
        previous(panic);
    }));
    terminal::enable_raw_mode()?;
    write_now(&setup(screen.mode()));
    Ok(Held {
        keys: keyboard(),
        screen,
        on_alternate,
    })
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        write_now(&reset(
            self.screen.mode(),
            self.on_alternate.load(Ordering::Relaxed),
        ));
    }
}

/// Write straight to the real stdout: the async handle may have buffered
/// writes we no longer own.
fn write_now(text: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    let _ = out.write_all(text.as_bytes());
    let _ = out.flush();
}

/// What waiting for a lost machine ended in.
#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    /// The delay is up. Try the session again.
    Retry,
    /// Somebody said to stop waiting, or there is no more waiting to do.
    GiveUp,
}

/// Sit out one delay between attempts to reach a session again.
///
/// The screen is left exactly as the session last painted it, with the mark
/// row saying what is going on: a connection that comes back finds the
/// terminal where it left it, and one that does not at least says why nothing
/// is happening. Nothing is written to the session, there being none.
///
/// The row is counted down rather than written once and left, and it says so
/// again when the delay is up and the attempt is out there
/// ([`status::waiting_notice`]). Both are for the same reason: this is the
/// only part of the screen that can move while a connection is gone, so a row
/// that does not is a client that has died as far as anybody can tell. And the
/// attempt is the half worth saying out loud, since reaching a machine that is
/// off takes as long as ssh takes to give up on it, which is most of the time
/// spent here on the wait that matters.
///
/// `lost_for` is how long ago the connection went, which the caller keeps
/// because it outlives one delay.
///
/// The keyboard is still read, because a wait nobody can leave is worse than
/// no wait at all. The mode key's detach is one way out, so that leaving a
/// session that is gone is the same gesture as leaving one that is there. The
/// other is Ctrl-C, which has nowhere else to go while there is no session to
/// send it to, and which is what a hand reaches for anyway.
pub async fn waiting(held: &mut Held, target: &str, delay: Duration, lost_for: Duration) -> Wait {
    let mut status = Status::new(target).lost();
    let began = tokio::time::Instant::now();
    let until = began + delay;
    // Which session this is about is already on the mark two columns to the
    // right, so the room goes to the wait itself: it no longer ends by
    // itself, and a row that does not say how to leave is a row that has to.
    // A notice too long for the terminal is not shown at all.
    let mut say = |left: Option<Duration>| {
        status.set_notice(&status::waiting_notice(lost_for + began.elapsed(), left));
        write_now(&status.repaint(terminal_size()));
    };

    let mut keys = KeyFilter::new(crate::client::attach::prefix());
    // The first tick is now, which is what paints the row on the way in.
    let mut tick = tokio::time::interval(TICK);
    let sleep = tokio::time::sleep_until(until);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => {
                say(None);
                return Wait::Retry;
            }
            _ = tick.tick() => say(Some(until.saturating_duration_since(tokio::time::Instant::now()))),
            typed = held.keys.recv() => match typed {
                // The keyboard reader is gone, which is stdin at end of file.
                // Nobody is going to press anything.
                None => return Wait::GiveUp,
                Some(chunk) => {
                    if chunk.contains(&INTERRUPT) {
                        return Wait::GiveUp;
                    }
                    if matches!(keys.filter(&chunk).action, Some(Action::Detach)) {
                        return Wait::GiveUp;
                    }
                }
            },
        }
    }
}

/// How often the row counts down. A second, because that is the unit it is
/// counting in.
const TICK: Duration = Duration::from_secs(1);

/// Ctrl-C, which while nothing is attached is a way out rather than a signal
/// to pass on.
const INTERRUPT: u8 = 0x03;

/// Run the attach loop until the client detaches, switches away, or the
/// session ends.
///
/// `mode` is where the keyboard starts, which is how a hop carries control
/// mode through the reattach.
/// `notice` is something the caller has to say about the key that led here,
/// which it has nowhere else to say it: between two attaches the terminal is
/// showing a session, and a line printed onto it would sit in the middle of
/// somebody's screen until the next repaint.
/// The name the session answers to at the end comes back with the outcome:
/// a rename from inside the session changes what the caller has to call it,
/// and this half of the client has no way to tell it any other way.
pub async fn run(
    held: &mut Held,
    session: Attached,
    target: &str,
    mode: Mode,
    notice: Option<&str>,
    popup: PopupFeed<'_>,
) -> Result<(Outcome, Option<String>)> {
    let PopupFeed {
        rows,
        group,
        asks,
        fresh,
    } = popup;
    let watching = session.read_only;
    let mut status = Status::new(target);
    if watching {
        status = status.watching();
    }
    status.set_mode(mode);
    status.set_group(group);
    if let Some(notice) = notice {
        status.set_notice(notice);
    }
    let screen = held.screen;
    let on_alternate = Arc::clone(&held.on_alternate);
    // One write, so there is never a frame showing an erased screen with no
    // mark on it. The screen the session before this one was in is left
    // here, so the flag is spent: what follows starts wherever this
    // session's own repaint puts the terminal.
    write_now(&format!(
        "{}{}",
        takeover(screen.mode(), on_alternate.swap(false, Ordering::Relaxed)),
        status.setup(terminal_size())
    ));
    let mut called = session_of(target).to_string();
    let outcome = pump(
        &mut held.keys,
        session,
        status,
        mode,
        Naming {
            host: host_of(target),
            called: &mut called,
        },
        Painting {
            screen,
            on_alternate,
            rows,
            asks,
            fresh,
        },
    )
    .await?;
    let renamed = called != session_of(target);
    Ok((outcome, renamed.then_some(called)))
}

/// Which session the client is sitting in, as the mark row names it: the
/// machine, which an attach never leaves, and the name, which a rename
/// moves under everything holding it.
struct Naming<'a> {
    host: Option<&'a str>,
    called: &'a mut String,
}

impl Naming<'_> {
    /// The two together, the way a target is spelled everywhere else.
    fn target(&self) -> String {
        match self.host {
            Some(host) => format!("{host}/{}", self.called),
            None => self.called.clone(),
        }
    }
}

/// The session's own name, out of a `host/name` target.
fn session_of(target: &str) -> &str {
    target.rsplit_once('/').map_or(target, |(_, name)| name)
}

/// The machine's, which a target that came without one does not have.
fn host_of(target: &str) -> Option<&str> {
    target.rsplit_once('/').map(|(host, _)| host)
}

/// How long a notice from the client itself stays on the row before the key
/// hints have it back. Long enough to read without looking for it.
const NOTICE_FOR: Duration = Duration::from_secs(5);

/// How often the view moves under a drag held against its edge.
///
/// How far it moves each time is [`scroll::step`], which ramps; this is the
/// clock both are spent against. Twenty a second, because every one of them
/// repaints the window, and a frame rate past what the eye reads as smooth is
/// bytes over ssh with nothing to show for them. Speed is worth having in the
/// step instead, where it costs a bigger jump and not another screenful.
const CHASE_EVERY: Duration = Duration::from_millis(50);

/// Ask the node for the session's screen, and note that it owes an erased,
/// homed one to paint it onto.
///
/// One call because they are one act. A dump starts painting wherever the
/// cursor is and never erases, so a screen asked for and painted where it fell
/// walks its own first rows off the top of the terminal, one `\r\n` at a time:
/// a full-screen program leaves the cursor at the bottom on its way out, and
/// what came back was the last lines of the session sitting under a blank half.
/// These were two statements once, at four call sites, and the one place the
/// second was missing showed nothing until somebody quit a pager.
async fn ask_for_the_screen(writer: &mut SessionWriter, owed: &mut usize) -> Result<()> {
    writer.resync().await?;
    *owed += 1;
    Ok(())
}

/// What the client says to take the mouse: report buttons and the moves made
/// with one held, in the SGR spelling.
///
/// `?1002h` is what makes a drag visible. `?1000h` alone reports a press and a
/// release and nothing between them, which is enough for a wheel and not enough
/// for a selection: the terminal has stopped selecting by then, so a client
/// that took the mouse and did not read the moves took the gesture away from
/// everybody.
const WHEEL_OURS: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";

/// And to give it back. In the other order, so nothing is left reporting in a
/// spelling that is no longer switched on.
const WHEEL_THEIRS: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";

/// What the row says about a copy that just happened.
///
/// Worth saying because there is nothing else to see: the highlight was already
/// there, the clipboard is somewhere else, and a terminal that refuses OSC 52
/// refuses it in silence. The count rather than the text, since the text is on
/// the screen two rows up.
fn copied_says(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => "copied".to_string(),
        lines => format!("copied {lines} lines"),
    }
}

/// Take the wheel, or give it back.
///
/// When that is, and when it is not, is [`wheel_is_ours`]. The arrow keys a
/// terminal would make of a notch with nobody reporting are not this function's
/// problem: alternate scroll is off for the whole run of attaches, in
/// [`crate::client::screen`].
///
/// What *is* its problem is that this state lives in two places, here and in
/// the terminal, and only one of them is ours to read. [`given_back`] switches
/// off every mode a session can ask for, and the mouse is one of those, so a
/// screen painted while the client held the wheel took the wheel with it and
/// this function had nothing to say about it: `wheel` still said ours, the
/// terminal had stopped reporting, and the first notch after a popup went back
/// to doing whatever the terminal does with one. So that path re-asserts
/// [`WHEEL_OURS`] by hand rather than clearing the flag and waiting for the
/// next frame, which would leave a window with the same silence in it.
async fn own_the_wheel(
    stdout: &mut Stdout,
    keys: &mut KeyFilter,
    wheel: &mut bool,
    ours: bool,
) -> Result<()> {
    if *wheel == ours {
        return Ok(());
    }
    *wheel = ours;
    keys.set_wheel(ours);
    stdout
        .write_all(if ours { WHEEL_OURS } else { WHEEL_THEIRS }.as_bytes())
        .await?;
    stdout.flush().await?;
    Ok(())
}

/// Chunks of keystrokes, read on a thread of its own.
///
/// `tokio::io::stdin` would do the same reads, but on a blocking pool
/// thread, and a read on one of those cannot be cancelled: the runtime
/// waits for it before it will shut down. Nobody types at a client whose
/// session has just ended, so that wait is forever, and the process hangs
/// on with the terminal already given back until a keystroke happens to
/// land. A thread nobody joins reads the same bytes and holds up nothing.
///
/// One of these lasts as long as the terminal is [`Held`], not as long as
/// an attach, because the read it is sitting in cannot be taken back. A
/// reader started per attach would leave the old one blocked on stdin
/// across a hop, and it learns its channel is closed only by finishing a
/// read first: it swallows the keystroke that told it, which is the one
/// that was meant to arrive in the session just switched to. That cost
/// exactly one key per hop, so walking the list took two presses of the
/// switch key instead of one.
fn keyboard() -> mpsc::Receiver<Vec<u8>> {
    // Enough to stay ahead of a paste arriving as one burst, and small
    // enough that a client not reading stops the thread rather than
    // growing a queue of stale keystrokes.
    let (typed, keys) = mpsc::channel(64);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match std::io::Read::read(&mut stdin, &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if typed.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });
    keys
}

/// Put the popup on the screen, or take the mark row back to saying what it
/// says without one.
///
/// One write, so there is never a frame with half a box on it, and the mark row
/// last: it is the thing that says the client has the keyboard, and it must not
/// be painted over by the box it is telling you about.
///
/// A window with no room for a box gets nothing back from [`Picker::draw`], and
/// the mark row takes the job over ([`Popped::Cramped`]). Without that, control
/// mode on a short terminal was a keyboard the client had taken and a tab that
/// changed nothing on the screen.
async fn draw_popup(
    stdout: &mut tokio::io::Stdout,
    popup: &mut Option<Popup>,
    status: &mut Status,
    restate: &mut bool,
) -> Result<()> {
    let Some(popup) = popup else {
        return Ok(());
    };
    let size = terminal_size();
    let box_drawn = popup.picker.draw(size);
    status.set_popup(if box_drawn.is_empty() {
        Popped::Cramped(popup.line())
    } else {
        Popped::Drawn
    });
    let drawn = format!("{box_drawn}{}", status.repaint(size));
    stdout.write_all(drawn.as_bytes()).await?;
    stdout.flush().await?;
    *restate = false;
    Ok(())
}

/// Everything the popup needs, which the caller owns and this half does not:
/// the rows, the narrowing they were built under, and the two ends of asking
/// for them again.
pub struct PopupFeed<'a> {
    pub rows: Rows,
    pub group: Option<&'a str>,
    pub asks: mpsc::Sender<()>,
    pub fresh: tokio::sync::watch::Receiver<Rows>,
}

/// What the pump paints on and what it paints there, which travel together
/// because the screen mode decides how all three are used.
struct Painting {
    screen: Screen,
    /// Whether the session has the terminal on a full-screen program's own
    /// screen, shared with the teardown and the panic hook.
    on_alternate: Arc<AtomicBool>,
    /// The rows the popup opens with.
    rows: Rows,
    /// Say the word and the caller goes and asks the machines what they are
    /// running again, answering on `fresh`.
    ///
    /// A press is the only thing that ever asks, here as everywhere else: the
    /// popup opens on whatever was true a moment ago and is corrected when the
    /// answer lands, rather than sitting on a machine that is asleep. The one
    /// that found nowhere to go asks too, or the first fruitless press would be
    /// the last one that ever asked.
    asks: mpsc::Sender<()>,
    /// Rows as of the last answer. Swapped in under the highlight by id, so a
    /// listing landing under an open popup does not slide it onto a different
    /// session.
    fresh: tokio::sync::watch::Receiver<Rows>,
}

/// The popup on the screen, and what the list in it is for.
///
/// Two verbs over one widget, and which list you entered from is what says
/// which: no row in either means two things, which is what an earlier shape of
/// this got wrong.
struct Popup {
    picker: Picker,
    what: Showing,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Showing {
    /// The sessions you can reach. Enter attaches.
    Sessions,
    /// Groups, to put the session on `row` into. Enter assigns.
    Moving { row: usize },
    /// Groups, to narrow to. Enter narrows.
    Narrowing,
}

const SESSION_HINTS: &str = "⏎ go  r name  m group  g show  n new  d detach";
const MOVE_HINTS: &str = "⏎ move   n new group   esc";
const NARROW_HINTS: &str = "⏎ show   esc";

impl Popup {
    fn sessions(rows: &Rows) -> Self {
        Self {
            picker: Picker::new("sessions", SESSION_HINTS, rows.sessions.clone(), rows.at),
            what: Showing::Sessions,
        }
    }

    /// The whole popup in one line, for a window with no room to draw the box.
    ///
    /// The title and the highlight, which is what the box says that this row
    /// cannot work out for itself: which list is open, and which row Enter
    /// would take.
    fn line(&self) -> String {
        match self.picker.chosen() {
            Some(row) => format!("{}: {}", self.picker.title(), row.label),
            // A list with nothing in it still has to say which list it is, or
            // the row goes blank in a mode that is still holding the keyboard.
            None => format!("{}: none", self.picker.title()),
        }
    }

    /// The mode this list is read in. The session list is control mode itself;
    /// the two group lists are their own, because the session keys must not
    /// reach them. Anything that has to put the keyboard back where it was
    /// asks here rather than assuming, since which list is up decides it.
    fn mode(&self) -> Mode {
        match self.what {
            Showing::Sessions => Mode::Control,
            Showing::Moving { .. } | Showing::Narrowing => Mode::Picking,
        }
    }

    /// The session row the popup is acting on, if it is on one.
    ///
    /// Narrowing, it is on none: the highlighted row there is a *group*, and
    /// its id indexes the groups. Answered with that id, `n` in the group list
    /// put whatever session happened to sit at that index in `Listed::at` into
    /// the new group, silently and on whatever machine it was on, because the
    /// write succeeded and there was nothing to say. `n` is live in both lists
    /// only because they share one key table; it is advertised in neither
    /// list's hints but the move list's.
    fn subject(&self) -> Option<usize> {
        match self.what {
            Showing::Moving { row } => Some(row),
            Showing::Sessions => self.picker.chosen().map(|row| row.id),
            Showing::Narrowing => None,
        }
    }
}

/// The name in `naming` is written back when a rename lands: the mark row
/// is not the only thing that has to follow it.
async fn pump(
    keyboard: &mut mpsc::Receiver<Vec<u8>>,
    session: Attached,
    mut status: Status,
    mode: Mode,
    naming: Naming<'_>,
    painting: Painting,
) -> Result<Outcome> {
    let Painting {
        screen,
        on_alternate,
        mut rows,
        asks,
        mut fresh,
    } = painting;
    let watching = session.read_only;
    let takes_pastes = session.paste;
    let scrolls = session.scroll;
    let renames = session.rename;
    let SessionHalves {
        mut reader,
        mut writer,
    } = session.split();
    let mut stdout = tokio::io::stdout();
    let mut winch = signal(SignalKind::window_change())?;
    let mut keys = KeyFilter::default();
    keys.set_mode(mode);
    // The key is the client's on a screen the client owns, whether or not
    // the host can answer for a window: a host that cannot is worth saying
    // out loud, and a key that quietly does nothing is the one thing worse
    // than not having it. Inline it is the session's, since the terminal
    // has the lines in its own buffer and its own wheel is better than
    // anything here.
    keys.set_scroll(screen.mode().owns_the_screen());
    // The wheel is a stricter question than the key, and asked for rather than
    // assumed: taking the mouse off the terminal costs it the bare-drag
    // selection, which nothing here can give back. So it wants somebody to have
    // said `mm config mouse client`, and a notch to have somewhere to go once
    // they have. A host that cannot answer for a window gets a sentence on the
    // row when the key is pressed, and keeps its wheel either way.
    let history =
        scrolls && screen.mode().owns_the_screen() && Settings::or_default().mouse == Mouse::Client;
    let mut output = Filter::new(screen);
    // The view over the session's history, while it is up.
    let mut scrolling: Option<Scrollback> = None;
    // The popup control mode puts on the screen, while it is up, and which of
    // the two lists it is showing.
    let mut popup: Option<Popup> = None;
    // Whether the client has mouse tracking on for itself, which it does
    // only while there is a history to look at and the session has asked
    // for no reports of its own.
    let mut wheel = false;
    // The row no longer says what the keys are doing. Held until there is a
    // safe moment to draw, the same as a mark the session cleared.
    let mut restate = false;
    // Whether the frame that repaints the screen on attach has been and
    // gone. Everything after it is the session speaking for itself.
    let mut painted = false;
    // Whether this attach still owes the popup its opening.
    //
    // Arriving in control mode is arriving with the popup up, since that is
    // what control mode looks like: an attach that started there and drew
    // nothing was a client holding the keyboard with nothing on the screen
    // saying so. It cannot be drawn until the repaint has been, though, because
    // that frame is a screen dump and paints by absolute coordinates from the
    // top, straight over anything already there.
    let mut greet = mode == Mode::Control;
    // Screens still owed for a resize, and so still to be painted onto one
    // wiped first. Counted rather than flagged because a drag across the
    // desktop asks more than once before the first answer arrives.
    let mut owed = 0usize;
    // When the notice on the row stops being worth showing. Already set if the
    // caller arrived with something to say, which needs the same few seconds
    // as one a key here put there.
    let mut notice_until: Option<tokio::time::Instant> = status
        .has_notice()
        .then(|| tokio::time::Instant::now() + NOTICE_FOR);
    // When the view next moves under a drag held against its edge, while one
    // is being held. Kept here rather than worked out per pass, or a session
    // printing steadily would push the moment away forever.
    let mut chase_at: Option<tokio::time::Instant> = None;
    // Notifications for the terminal, waiting for a safe moment to be
    // written, and the rule for which of them get one.
    let mut pending = String::new();
    let bells = Bells::new(naming.host);

    loop {
        tokio::select! {
            typed = keyboard.recv() => {
                let Some(typed) = typed else {
                    return Ok(Outcome::Detached);
                };
                // Round the chunk until it is used up. Only a popup move hands
                // any of it back (see `Keystrokes::rest`), and it has to:
                // two writes a moment apart arrive as one read, so `tab` then
                // `Enter` is a single chunk and stopping at the move would
                // throw away the key that commits.
                let mut chunk = typed;
                'chunk: loop {
                let keystrokes = keys.filter(&chunk);
                chunk = keystrokes.rest.clone();
                // A viewer's keystrokes go nowhere. The node drops them anyway,
                // which is what makes the promise worth anything; not sending
                // them keeps a held key off the wire and out of the session's
                // idle time, which is drawn in every listing.
                if !keystrokes.forward.is_empty() && !watching {
                    writer.send_input(&keystrokes.forward).await?;
                }
                let was = status.mode();
                if keystrokes.mode != was {
                    status.set_mode(keystrokes.mode);
                    restate = true;
                }
                // The mode key on its own, which is what control mode looks
                // like: the popup is the mode rather than something the mode
                // can show, so it goes up on the way in and not on the first
                // key after it. Asked for again in the same breath, because a
                // press is the only thing that ever asks.
                if keystrokes.mode == Mode::Control && was != Mode::Control && popup.is_none() {
                    let _ = asks.try_send(());
                    popup = Some(Popup::sessions(&rows));
                    draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                }
                settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
                match keystrokes.action {
                    // Detached either way, so that the node does not hold an
                    // attachment for a client that has gone elsewhere.
                    Some(Action::Detach) => {
                        writer.detach().await?;
                        return Ok(Outcome::Detached);
                    }
                    Some(Action::Switch(motion)) => {
                        writer.detach().await?;
                        return Ok(Outcome::Switch(motion));
                    }
                    // Every hop in a viewing run is another view, so the
                    // session this would start is one you could not type
                    // into. Said on the row rather than swallowed, the same
                    // as a rename asked for from here.
                    Some(Action::New) if watching => {
                        status.set_notice("watching, so nothing here can start a session");
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                    Some(Action::New) => {
                        writer.detach().await?;
                        return Ok(Outcome::New);
                    }
                    // The popup, which is what control mode looks like. Moving
                    // the highlight is local: walking three sessions used to be
                    // three detaches and three reattaches over ssh just to see
                    // what each one was, and is now one, on the Enter.
                    // Closing is the teardown below, which every other way out
                    // of the mode goes through too. Nothing to do here but stay
                    // out of its way: opening a popup to close it would put the
                    // box back on the screen.
                    // Esc out of a group list is out of that list and not out
                    // of the popup: it was opened over the session list, and
                    // both lists' hints say so. The session picker was
                    // replaced rather than stacked under, so coming back is
                    // building it again, on the row the gesture started from
                    // where there was one. Out of the session list it is the
                    // teardown below, like every other way of closing the box.
                    Some(Action::Pick(Pick::Cancel)) => {
                        if let Some(up) = popup.as_mut()
                            && up.what != Showing::Sessions
                        {
                            let was = match up.what {
                                Showing::Moving { row } => Some(row),
                                _ => None,
                            };
                            *up = Popup::sessions(&rows);
                            if let Some(row) = was {
                                up.picker.point_at(row);
                            }
                            draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                        }
                    }
                    Some(Action::Pick(pick)) => {
                        // Every press asks, whether or not it moved anywhere.
                        // Asking only after a landing meant the first press
                        // that found nothing was the last one that ever asked,
                        // so a machine with one session on it when the run
                        // started stayed that way as far as this was concerned.
                        let _ = asks.try_send(());
                        let up = popup.get_or_insert_with(|| Popup::sessions(&rows));
                        match pick {
                            Pick::Up => up.picker.up(),
                            Pick::Down => up.picker.down(),
                            Pick::NextGroup => up.picker.next_heading(true),
                            Pick::PreviousGroup => up.picker.next_heading(false),
                            // Off to the group list, over the session list
                            // rather than instead of it: Esc there comes back
                            // to where the gesture started.
                            Pick::Move | Pick::Groups => {
                                let Some(row) = up.picker.chosen().map(|row| row.id) else {
                                    // No row to act on, and the mode has
                                    // already moved: `KeyFilter::after` reads
                                    // the key, not what this arm decides about
                                    // it, so leaving here without putting it
                                    // back left the group table over the
                                    // session list. Same shape as the bug
                                    // `Popup::mode` was added for.
                                    let back = up.mode();
                                    keys.set_mode(back);
                                    status.set_mode(back);
                                    restate = true;
                                    continue;
                                };
                                let (title, hints, what) = if pick == Pick::Move {
                                    let name = up
                                        .picker
                                        .chosen()
                                        .map(|row| row.label.clone())
                                        .unwrap_or_default();
                                    (
                                        format!("move \"{name}\" to"),
                                        MOVE_HINTS,
                                        Showing::Moving { row },
                                    )
                                } else {
                                    ("groups".to_string(), NARROW_HINTS, Showing::Narrowing)
                                };
                                *up = Popup {
                                    picker: Picker::new(title, hints, rows.groups.clone(), 0),
                                    what,
                                };
                                keys.set_mode(Mode::Picking);
                                status.set_mode(Mode::Picking);
                            }
                            Pick::Go => {
                                let chosen = up.picker.chosen().map(|row| row.id);
                                let what = up.what;
                                let subject = up.subject();
                                let Some(chosen) = chosen else { continue };
                                status.set_popup(Popped::None);
                                writer.detach().await?;
                                return Ok(match (what, subject) {
                                    (Showing::Sessions, _) => {
                                        Outcome::Chose(Chose::Go(chosen))
                                    }
                                    (Showing::Moving { .. }, Some(session)) => {
                                        Outcome::Chose(Chose::Move { session, group: chosen })
                                    }
                                    (Showing::Moving { .. }, None) => Outcome::Detached,
                                    (Showing::Narrowing, _) => {
                                        Outcome::Chose(Chose::Focus(chosen))
                                    }
                                });
                            }
                            // Taken off the screen by the teardown below,
                            // which every other way out of the mode goes
                            // through too. Unreachable: matched above.
                            Pick::Cancel => {}
                        }
                        draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                    }
                    // Naming a group, at the prompt the rename and the search
                    // already share. Enter creates it and puts the session in
                    // it, necessarily: a group is a set of live sessions, so an
                    // empty one cannot exist.
                    // A new group is a session put in one, since a group with
                    // nothing in it cannot exist. So a list with no session
                    // under the highlight has nothing to name, and the prompt
                    // is refused rather than opened: at the Enter a name has
                    // been typed and refusing it there throws the typing away.
                    Some(Action::GroupName(Rename::Open))
                        if popup.as_ref().is_none_or(|up| up.subject().is_none()) =>
                    {
                        keys.stop_typing();
                        // And back to the mode the key arrived in, which
                        // `KeyFilter::after` has already left at `Mode::Rename`
                        // for a prompt that is not going to open. A mode with
                        // no prompt behind it falls through to the session
                        // table, so the group list was left on the screen with
                        // `m`, `d` and `n` live over it: `m` there reads a
                        // group row as a session, which is the thing refusing
                        // this key exists to stop.
                        let back = popup.as_ref().map_or(Mode::Focus, Popup::mode);
                        keys.set_mode(back);
                        status.set_mode(back);
                        status.set_grouping(None);
                        status.set_notice("no session here to put in a group");
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                    }
                    Some(Action::GroupName(step)) => {
                        match step {
                            Rename::Open | Rename::Typed => {
                                status.set_grouping(keys.wanted_group());
                            }
                            Rename::Cancel => status.set_grouping(None),
                            Rename::Run => {
                                let name = keys.wanted_group().unwrap_or_default();
                                keys.stop_typing();
                                status.set_grouping(None);
                                let session = popup.as_ref().and_then(Popup::subject);
                                if let Some(session) = session
                                    && !name.trim().is_empty()
                                {
                                    status.set_popup(Popped::None);
                                    writer.detach().await?;
                                    return Ok(Outcome::Chose(Chose::NewGroup {
                                        session,
                                        name,
                                    }));
                                }
                            }
                        }
                        restate = true;
                        draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                    }
                    // A host from before the view existed answers for no
                    // window, so there is nothing to open. Said on the row
                    // rather than swallowed, because a key that does
                    // nothing and says nothing reads as a broken client.
                    // The keyboard goes back where it was: a mode with no
                    // view behind it would say `scroll` on the row and
                    // take every key you typed.
                    Some(Action::Scroll(_)) if !scrolls => {
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                        status.set_notice(
                            "this host is too old to scroll back; `mm restart` there",
                        );
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                    // Back to the live screen, which the view has been
                    // drawing over: the node's model is the only place it
                    // still exists, and it is painted onto an erased screen
                    // the way a resize is.
                    Some(Action::Scroll(Scroll::Leave)) => {
                        scrolling = None;
                        status.set_scrolled(None);
                        ask_for_the_screen(&mut writer, &mut owed).await?;
                        restate = true;
                    }
                    // Typing, and what typing turns into. The needle lives
                    // in the key filter until Enter, so all of this does is
                    // keep the row saying what is in it.
                    Some(Action::Find(found)) if !scrolls => {
                        keys.stop_typing();
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                        let _ = found;
                        status.set_notice(
                            "this host is too old to search; `mm restart` there",
                        );
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                    Some(Action::Find(found)) => {
                        let view = scrolling
                            .get_or_insert_with(|| Scrollback::new(session_size()));
                        match found {
                            Find::Open | Find::Typed => {
                                status.set_prompt(keys.needle());
                            }
                            Find::Cancel => status.set_prompt(None),
                            Find::Run => {
                                let needle = keys.needle().unwrap_or_default();
                                keys.stop_typing();
                                status.set_prompt(None);
                                writer.find(&needle).await?;
                            }
                            // Walking the matches is local: every one of
                            // them came back with the search.
                            Find::Next | Find::Previous => {
                                view.step(found == Find::Next);
                                status.set_scrolled(Some(view.offset()));
                                status.set_searching(view.searching());
                                let wanted = view.wanted();
                                let painted = view.paint();
                                if let Some(request) = wanted {
                                    writer.view(&request).await?;
                                }
                                stdout.write_all(painted.as_bytes()).await?;
                            }
                        }
                        stdout
                            .write_all(status.repaint(terminal_size()).as_bytes())
                            .await?;
                        stdout.flush().await?;
                    }
                    Some(Action::Scroll(motion)) => {
                        let view = scrolling
                            .get_or_insert_with(|| Scrollback::new(session_size()));
                        match motion {
                            Scroll::Up(lines) => view.up(lines),
                            Scroll::Down(lines) => view.down(lines),
                            Scroll::PageUp => view.page_up(),
                            Scroll::PageDown => view.page_down(),
                            Scroll::Top => view.top(),
                            Scroll::Bottom => view.bottom(),
                            // Handled above, where the view can be dropped
                            // without holding a borrow of it.
                            Scroll::Leave => {}
                        }
                        // Scrolled all the way back down, which is somebody
                        // arriving at the live screen rather than asking to
                        // look at a window that happens to sit on it. The view
                        // is a picture of where the session was, so sitting in
                        // it at the bottom is the one place it says nothing the
                        // session would not say better, and leaving costs an
                        // Esc nobody thinks to press. Only on the way down:
                        // opening the view is a move up made from the bottom,
                        // and a history shorter than the screen never leaves
                        // it, so reading this off the offset alone would open
                        // the view and shut it in the same notch.
                        let landed = view.at_bottom()
                            && matches!(
                                motion,
                                Scroll::Down(_) | Scroll::PageDown | Scroll::Bottom
                            );
                        if landed {
                            scrolling = None;
                            status.set_scrolled(None);
                            keys.set_mode(Mode::Focus);
                            status.set_mode(Mode::Focus);
                            ask_for_the_screen(&mut writer, &mut owed).await?;
                            restate = true;
                            settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                                .await?;
                        } else {
                            let wanted = view.wanted();
                            let painted = view.paint();
                            status.set_scrolled(Some(view.offset()));
                            if let Some(request) = wanted {
                                writer.view(&request).await?;
                            }
                            stdout.write_all(painted.as_bytes()).await?;
                            stdout
                                .write_all(status.repaint(terminal_size()).as_bytes())
                                .await?;
                            stdout.flush().await?;
                        }
                    }
                    // A drag, which only means anything where there is a view
                    // to drag over. On a host too old for one the client never
                    // took the mouse in the first place, so this cannot arrive;
                    // the arm is here because the reports and the window are
                    // switched on by the same flag and nothing should depend on
                    // remembering that.
                    Some(Action::Select(_)) if !scrolls => {
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                    }
                    Some(Action::Select(what)) => {
                        let view = scrolling
                            .get_or_insert_with(|| Scrollback::new(session_size()));
                        match what {
                            Select::From(at) => view.select_from(at),
                            Select::To(at) => view.select_to(at),
                            Select::Word(at) => view.select_word(at),
                            Select::Line(at) => view.select_line(at),
                            // Letting go changes nothing about what is
                            // selected. It is the moment the selection becomes
                            // worth having, which is a different thing, and
                            // the moment the hand stops asking for the view.
                            Select::Done => view.let_go(),
                        }
                        let copied = match what {
                            Select::Done => view.copied(),
                            _ => None,
                        };
                        // A gesture that is over, made on the live screen: the
                        // press opened the view under it and there is nothing
                        // left down here to look at. Whether it copied anything
                        // or not, since the two ways it ends are a click that
                        // selected nothing and a drag that has been taken, and
                        // both are somebody done with the mouse.
                        //
                        // Leaving somebody in a paused picture of the session
                        // they were looking at is what makes the gesture read
                        // as a client that has died: the session goes on
                        // running and stops being painted, and the keys they
                        // type next go to the view rather than to the shell,
                        // with only an Esc nobody thinks to press to get out of
                        // it. Scrolled back, the same release is somebody
                        // taking a line out of the history they are reading,
                        // and there the view is the point and stays.
                        //
                        // A copy still owed is neither, and waits: the block it
                        // needs is on its way, and `Update::View` finishes it.
                        let done = matches!(what, Select::Done)
                            && view.at_bottom()
                            && (copied.is_some() || !view.selecting());
                        let wanted = if done { None } else { view.wanted() };
                        let painted = if done { String::new() } else { view.paint() };
                        let offset = view.offset();
                        // A drag that has reached an edge and stayed there
                        // reports nothing more, so from here the view moves on
                        // a clock of its own.
                        chase_at = (!done && view.chasing())
                            .then(|| tokio::time::Instant::now() + CHASE_EVERY);
                        if let Some(request) = wanted {
                            writer.view(&request).await?;
                        }
                        if let Some(text) = &copied {
                            stdout
                                .write_all(clipboard::to_terminal(text).as_bytes())
                                .await?;
                            status.set_notice(&copied_says(text));
                            notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        }
                        if done {
                            scrolling = None;
                            status.set_scrolled(None);
                            keys.set_mode(Mode::Focus);
                            status.set_mode(Mode::Focus);
                            ask_for_the_screen(&mut writer, &mut owed).await?;
                            restate = true;
                            settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                                .await?;
                        } else {
                            status.set_scrolled(Some(offset));
                            stdout.write_all(painted.as_bytes()).await?;
                            stdout
                                .write_all(status.repaint(terminal_size()).as_bytes())
                                .await?;
                            stdout.flush().await?;
                        }
                    }
                    // The same shape as a search, and the same reason for
                    // it: the name lives in the key filter until Enter, so
                    // all there is to do until then is keep the row saying
                    // what is in it.
                    Some(Action::Rename(_)) if watching || !renames => {
                        keys.stop_typing();
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                        status.set_renaming(None);
                        status.set_notice(if watching {
                            "watching, so nothing here can be renamed"
                        } else {
                            "this host is too old to rename from here; `mm rename` instead"
                        });
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                    Some(Action::Rename(step)) => {
                        match step {
                            Rename::Open | Rename::Typed => {
                                status.set_renaming(keys.wanted_name());
                            }
                            Rename::Cancel => status.set_renaming(None),
                            Rename::Run => {
                                let wanted = keys.wanted_name().unwrap_or_default();
                                keys.stop_typing();
                                status.set_renaming(None);
                                // The row the popup is on, which may not be the
                                // session at the other end of this stream, and
                                // `tag::RENAME` renames that one by design. So
                                // it goes back to the caller, which can reach
                                // the machine holding it.
                                match popup.as_ref() {
                                    // A popup is up, so it names the row, and a
                                    // list with no session under the highlight
                                    // names nothing: this is the one place the
                                    // two meanings of "no session" have to stay
                                    // apart, or a prompt opened over the group
                                    // list would rename the session at the
                                    // other end of the stream instead.
                                    Some(up) => match up.subject() {
                                        Some(session) if !wanted.trim().is_empty() => {
                                            status.set_popup(Popped::None);
                                            writer.detach().await?;
                                            return Ok(Outcome::Chose(Chose::Named {
                                                session,
                                                to: wanted,
                                            }));
                                        }
                                        // Typed at a list with nothing under
                                        // the highlight, which is a name typed
                                        // for nothing. Said, because the box
                                        // closes on its own here and a name
                                        // that went nowhere in silence reads as
                                        // a rename that worked.
                                        None => {
                                            status.set_notice("no session here to rename");
                                            notice_until =
                                                Some(tokio::time::Instant::now() + NOTICE_FOR);
                                        }
                                        Some(_) => {}
                                    },
                                    // No popup, so this is a client driving the
                                    // prompt without one: the session at the
                                    // other end of the stream is the only one
                                    // it could mean. Nothing is said here, the
                                    // row keeps the old name until the host
                                    // answers.
                                    None => writer.rename(&wanted).await?,
                                }
                            }
                        }
                        // Through `settle` rather than written on the spot,
                        // unlike the search: that one runs with the view
                        // owning the screen, and this one runs with the
                        // session still painting on it.
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                    // Nothing to hand a session this client cannot type into,
                    // and the clipboard is not worth reading to find that out.
                    Some(Action::Paste) if watching => {
                        status.set_notice("watching, so there is nothing to paste into");
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                    }
                    Some(Action::Paste) => {
                        // The key as the terminal spelled it, since it goes
                        // to the session unchanged when there turns out to
                        // be nothing on the clipboard to paste.
                        let key = keys.spelling().to_vec();
                        let pasted = paste(
                            &mut reader,
                            &mut writer,
                            &mut stdout,
                            &mut output,
                            &mut status,
                            takes_pastes,
                            &key,
                        ).await?;
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        // The paste had the writing half to itself, so a
                        // switch swallowed while it ran is only now
                        // answerable.
                        if output.take_switched() {
                            ask_for_the_screen(&mut writer, &mut owed).await?;
                        }
                        // The same discipline as everywhere else: never
                        // into the middle of a sequence the session is
                        // part way through.
                        restate = true;
                        if output.at_boundary() {
                            stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                            stdout.flush().await?;
                            restate = false;
                        }
                        if let Pasted::Ended(outcome) = pasted {
                            return Ok(outcome);
                        }
                    }
                    None => {}
                }
                // A mode that is no longer one of the popup's is a popup that
                // has to come off the screen, whichever key did it. Keyed off
                // the mode rather than off an action, because most of the ways
                // out are not actions at all: an unbound key, a mistyped mode
                // key, an Esc. The client has no copy of what the box covered,
                // so the node's model is the only place it still exists and it
                // is painted back the way a resize is, which is the same trip
                // leaving the view already makes.
                // `Mode::Rename` keeps it: both prompts opened from the popup
                // are a step of a gesture that started there, and the row being
                // named or grouped is the highlighted one. Taking the box away
                // under the prompt would leave nothing saying which.
                if popup.is_some()
                    && !matches!(
                        status.mode(),
                        Mode::Control | Mode::Picking | Mode::Rename
                    )
                {
                    popup = None;
                    status.set_popup(Popped::None);
                    ask_for_the_screen(&mut writer, &mut owed).await?;
                    restate = true;
                    settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
                }
                // The session may have asked for the mouse, or given it back,
                // and the wheel goes with that: a program that wants reports
                // gets every one of them, the wheel included.
                let ours = wheel_is_ours(history, output.session_mouse());
                own_the_wheel(&mut stdout, &mut keys, &mut wheel, ours).await?;
                if chunk.is_empty() {
                    break 'chunk;
                }
                }
            }
            // A listing has landed. Swapped in under the highlight rather than
            // reopening the popup, so a session ending three rows up does not
            // move what Enter would take.
            landed = fresh.changed() => {
                if landed.is_err() {
                    continue;
                }
                rows = fresh.borrow_and_update().clone();
                if let Some(up) = popup.as_mut() {
                    if up.what == Showing::Sessions {
                        up.picker.replace(rows.sessions.clone(), rows.at);
                    } else {
                        // A group list has no such row: `at` is a session's.
                        up.picker.replace(rows.groups.clone(), 0);
                    }
                    draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                }
            }
            update = reader.next() => match update? {
                Update::Output(bytes) => {
                    // The repaint, and it needs the screen underneath it
                    // blank: the dump paints by absolute coordinates from
                    // the top. A screen of the client's own was erased in
                    // the takeover; the terminal's own is rolled into its
                    // scrollback instead, after any history, which is
                    // written as it arrives.
                    if !painted {
                        let before = screen.mode().before_repaint(terminal_size());
                        stdout.write_all(before.as_bytes()).await?;
                    }
                    let bytes = output.feed(&bytes);
                    // Fed to the filter either way, so its parser stays in
                    // step with the byte stream, but not written while
                    // something of the client's owns the screen. The view is
                    // showing history; the box is drawn over the session and
                    // has no way to stay on top of it, since a line printed
                    // scrolls the screen and takes the rows of the box already
                    // drawn up with it. Redrawing after every chunk leaves one
                    // copy of the box per line printed: a terminal composes
                    // nothing, and the client is not the emulator here, so the
                    // only surface either of these can own is the whole one.
                    // The session goes on running behind both, and both are put
                    // back by the resync that closing them asks for, so what
                    // pauses is the picture and never the program.
                    if scrolling.is_none() && popup.is_none() {
                        stdout.write_all(&bytes).await?;
                    }
                    on_alternate.store(output.on_alternate(), Ordering::Relaxed);
                    // The session may have just asked for the mouse, or
                    // given it back. Only between sequences, like the mark.
                    if output.at_boundary() {
                        let ours = wheel_is_ours(history, output.session_mouse());
                        own_the_wheel(&mut stdout, &mut keys, &mut wheel, ours).await?;
                    }
                    // A screen switch went no further than this client, so
                    // the terminal is still showing the screen the session
                    // has just left. Ask for the other one, which exists
                    // only in the node's model of the session. The frame
                    // that repaints on attach is a dump like the answer
                    // would be, and asking for another of those is a round
                    // trip that paints the same screen twice. Inline this
                    // never fires: the terminal made the switch itself and
                    // kept both screens.
                    if output.take_switched() && painted {
                        ask_for_the_screen(&mut writer, &mut owed).await?;
                    }
                    painted = true;
                    // Only between sequences: a repaint written into the
                    // middle of one would corrupt it. Whatever cleared the
                    // mark stays noted until there is a safe moment.
                    if output.at_boundary() {
                        restate |= output.take_dirty();
                    }
                    settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
                    // The screen is up, so the box has something to sit on. The
                    // rows are the ones the caller handed over, which it built
                    // after whatever brought us back here, so this opens on
                    // what is true rather than asking and waiting.
                    if std::mem::take(&mut greet) {
                        // And ask, the same as the key that opens one does. The
                        // rows handed over are whatever the caller knew, which
                        // after a slow fan-out is nothing at all: opening on
                        // them without asking left an empty box that only the
                        // next keypress would fill, and an empty box is one
                        // there is nothing obvious to press in.
                        let _ = asks.try_send(());
                        popup = Some(Popup::sessions(&rows));
                        draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                    }
                }
                // The screen we asked for. Its own switches are how a dump
                // paints both buffers, so they are swallowed and dropped
                // rather than answered with another request.
                Update::Screen(bytes) => {
                    // Not onto an open box: this is a whole screen and would
                    // take it with it. Closing the box asks for another, so
                    // what is dropped here is painted a moment later anyway.
                    let ours = popup.is_none();
                    if owed > 0 {
                        owed -= 1;
                        if ours {
                            // What the dump replays is what the program has
                            // on, so what it turned off since goes off here:
                            // see `given_back`.
                            stdout.write_all(given_back().as_bytes()).await?;
                            // Which includes the mouse, and the client's hold
                            // on it is not the session's to lose.
                            if wheel {
                                stdout.write_all(WHEEL_OURS.as_bytes()).await?;
                            }
                            stdout.write_all(REGROWN.as_bytes()).await?;
                        }
                    }
                    let bytes = output.feed(&bytes);
                    if ours {
                        stdout.write_all(&bytes).await?;
                    }
                    on_alternate.store(output.on_alternate(), Ordering::Relaxed);
                    output.take_switched();
                    output.take_dirty();
                    // The dump put the session's own screen back, mark and
                    // region included, so both are ours to draw again.
                    restate = true;
                    settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
                }
                // Straight out, ahead of the roll that scrolls it into the
                // terminal's own scrollback. Not through the filter: these
                // are lines the node rendered, not the session speaking, so
                // there is no title to prefix and no mark to put back.
                Update::History(bytes) => {
                    stdout.write_all(&bytes).await?;
                }
                // Where the search found what it was looking for. The view
                // jumps to the first match above where it is sitting, and
                // asks for the block around it.
                Update::Found(found) => {
                    if let Some(view) = scrolling.as_mut() {
                        view.found(found);
                        status.set_scrolled(Some(view.offset()));
                        status.set_searching(view.searching());
                        let wanted = view.wanted();
                        let painted = view.paint();
                        if let Some(request) = wanted {
                            writer.view(&request).await?;
                        }
                        stdout.write_all(painted.as_bytes()).await?;
                        stdout
                            .write_all(status.repaint(terminal_size()).as_bytes())
                            .await?;
                        stdout.flush().await?;
                    }
                }
                // A block of the history. Dropped if the view has been
                // closed since it was asked for: what is on the screen is
                // the session again, and painting lines over it would be a
                // window nobody is looking at.
                Update::View(window) => {
                    // The copy a release could not be answered out of an empty
                    // block, and whether the gesture it belongs to is over.
                    let mut copied = None;
                    let mut done = false;
                    if let Some(view) = scrolling.as_mut() {
                        view.take(window);
                        copied = view.owed_copy();
                        done = copied.is_some() && view.at_bottom();
                        // The block was asked for around where the window was
                        // before the host said how much history there is, so a
                        // move made before the first answer may have been
                        // brought back inside it and left the block covering
                        // somewhere else. Asking again is a no-op when it does
                        // cover, which is every case but that one.
                        if !done
                            && let Some(request) = view.wanted()
                        {
                            writer.view(&request).await?;
                        }
                        if !done {
                            status.set_scrolled(Some(view.offset()));
                            stdout.write_all(view.paint().as_bytes()).await?;
                            stdout
                                .write_all(status.repaint(terminal_size()).as_bytes())
                                .await?;
                            stdout.flush().await?;
                        }
                    }
                    if let Some(text) = &copied {
                        stdout
                            .write_all(clipboard::to_terminal(text).as_bytes())
                            .await?;
                        status.set_notice(&copied_says(text));
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                    }
                    // The drag was over before its lines arrived, so this is
                    // where it ends: the same handing back of the screen the
                    // release would have done had it been able to answer.
                    if done {
                        scrolling = None;
                        status.set_scrolled(None);
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                        ask_for_the_screen(&mut writer, &mut owed).await?;
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                }
                // What the session is called now. The mark row is the only
                // place on the screen that says so, and the window's name
                // goes with it, for a session that has set no title of its
                // own and so is showing the target there.
                Update::Renamed(answer) => {
                    match answer {
                        Renamed::Name(name) => {
                            *naming.called = name;
                            status.set_target(&naming.target());
                            pending.push_str(&status.retitle());
                            status.set_notice(&format!("renamed to {}", naming.called));
                        }
                        Renamed::Refused(why) => status.set_notice(&why),
                    }
                    notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                    restate = true;
                    settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                        .await?;
                }
                // A bell in one of this machine's other sessions. The
                // terminal is asked to raise it, and the row says which
                // session it was, for a terminal that raises nothing.
                Update::Event(hosted) => {
                    if let Some(rung) = bells.ring(&hosted) {
                        pending.push_str(&rung.escape);
                        status.set_notice(&rung.notice);
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                }
                // Answered from here rather than inside the reader, which
                // does not hold the writing half to answer with.
                Update::Ping => writer.pong().await?,
                Update::Exited(code) => return Ok(Outcome::Exited(code)),
                // Nothing to do here. This client's screen is the terminal's,
                // which is whatever size the terminal is, and the node clips
                // what it paints to the size it took. It is a client keeping a
                // screen of its own that has to hear this.
                Update::Resized(_) => {}
                Update::Disconnected => return Ok(Outcome::Disconnected),
            },
            _ = winch.recv() => {
                let size = terminal_size();
                writer.resize(status::session_size(size)).await?;
                // The new geometry moved the mark and the region with it,
                // and the region goes first because the screen asked for
                // below is painted with newlines that would scroll against
                // the old fence.
                stdout.write_all(status.repaint(size).as_bytes()).await?;
                stdout.flush().await?;
                // The view is showing lines rather than the session, so it
                // repaints itself at the new size. A screen asked for here
                // would be painted over it and then be gone when the view
                // closes onto an erased screen anyway.
                if let Some(view) = scrolling.as_mut() {
                    view.resize(status::session_size(size));
                    let wanted = view.wanted();
                    let painted = view.paint();
                    if let Some(request) = wanted {
                        writer.view(&request).await?;
                    }
                    stdout.write_all(painted.as_bytes()).await?;
                    stdout.write_all(status.repaint(size).as_bytes()).await?;
                    stdout.flush().await?;
                    continue;
                }
                // And a popup is a surface of ours too, so it is drawn again at
                // the new geometry rather than left where it was: the screen
                // asked for below is dropped while a box is up, so nothing else
                // was going to touch it.
                //
                // Onto an erased screen, which is the half `Picker::cleared`
                // cannot do: it blanks the rows the box gives up, in the
                // columns the *new* box sits at, on the stated assumption that
                // a resize repaints everything. A resize is also the one thing
                // that moves the box sideways, so a narrower window left the
                // old box's right edge standing in a column the new one never
                // writes. Everything on this screen is at the old geometry
                // anyway, and what is under the box is put back by the resync
                // that closing it asks for.
                if popup.is_some() {
                    stdout.write_all(REGROWN.as_bytes()).await?;
                    draw_popup(&mut stdout, &mut popup, &mut status, &mut restate).await?;
                    stdout.write_all(status.repaint(size).as_bytes()).await?;
                    stdout.flush().await?;
                    continue;
                }
                ask_for_the_screen(&mut writer, &mut owed).await?;
            }
            // A drag is being held against the top or bottom of the window,
            // which is a hand asking for the lines past it. The pointer is
            // holding still, so the terminal has nothing more to say about it
            // and the view has to move itself.
            _ = expire(chase_at) => {
                chase_at = None;
                let Some(view) = scrolling.as_mut() else { continue };
                match view.chase() {
                    Chased::Moved => {
                        let wanted = view.wanted();
                        let painted = view.paint();
                        let offset = view.offset();
                        if let Some(request) = wanted {
                            writer.view(&request).await?;
                        }
                        status.set_scrolled(Some(offset));
                        stdout.write_all(painted.as_bytes()).await?;
                        stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                        stdout.flush().await?;
                        chase_at = Some(tokio::time::Instant::now() + CHASE_EVERY);
                    }
                    // The oldest line there is, or the live screen. Nothing to
                    // say: the view is against the end of what exists and it
                    // is showing that.
                    Chased::Stopped => {}
                    Chased::Full => {
                        // The move that hit the limit still moved: it is walked
                        // back to what a copy can carry, not undone, so the
                        // window and the highlight are both a step from what
                        // was last painted. Nothing else is going to paint
                        // them, since the hand is holding still and there are
                        // no more reports coming, and the notice would then be
                        // describing a screen showing something else.
                        let painted = view.paint();
                        status.set_scrolled(Some(view.offset()));
                        stdout.write_all(painted.as_bytes()).await?;
                        status.set_notice(&format!(
                            "{} lines is as much as one copy takes",
                            scroll::COPY_LINES
                        ));
                        notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
                        restate = true;
                        settle(&mut stdout, &output, &status, &mut pending, &mut restate)
                            .await?;
                    }
                }
            }
            // A notice the client put on the row has been up long enough.
            _ = expire(notice_until) => {
                notice_until = None;
                status.clear_notice();
                restate = true;
                settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
            }
        }
    }
}

/// Write what has been held back until it was safe to write, and flush.
///
/// Both things here would corrupt a sequence the session is halfway through
/// if they landed in the middle of one, so both wait for a boundary: the
/// mark, which a clear or a full-screen program takes away, and a
/// notification for the terminal, which arrives whenever another session
/// feels like ringing.
async fn settle(
    stdout: &mut Stdout,
    output: &Filter,
    status: &Status,
    pending: &mut String,
    restate: &mut bool,
) -> Result<()> {
    if output.at_boundary() {
        if !pending.is_empty() {
            stdout.write_all(pending.as_bytes()).await?;
            pending.clear();
        }
        if *restate {
            stdout
                .write_all(status.repaint(terminal_size()).as_bytes())
                .await?;
            *restate = false;
        }
    }
    stdout.flush().await?;
    Ok(())
}

/// What a session next door is allowed to say to this terminal.
struct Bells {
    /// The machine as the person typed it, which is what a notification
    /// should call it: `deploy@prod-1` is not the name that machine has for
    /// itself, but it is the one they would recognise.
    host: Option<String>,
    cooldown: notify::Cooldown,
}

/// A notification on its way to the terminal.
struct Rung {
    escape: String,
    notice: String,
}

impl Bells {
    fn new(host: Option<&str>) -> Self {
        Self {
            host: host.map(str::to_string),
            cooldown: notify::Cooldown::default(),
        }
    }

    /// What to write for an event, or `None` for one not worth interrupting
    /// anybody over.
    fn ring(&self, hosted: &HostedEvent) -> Option<Rung> {
        // Asked every time rather than once at attach, so `mm config notify off`
        // takes hold in the session you are already sitting in.
        if !notify::to_terminal() {
            return None;
        }
        let host = self.host.as_deref().unwrap_or(&hosted.host);
        let notification = notify::worth_interrupting(host, &hosted.event)?;
        if !self
            .cooldown
            .allow(&format!("{host}/{}", hosted.event.session))
        {
            return None;
        }
        Some(Rung {
            escape: notify::escape(&notification),
            notice: notify::summary(&hosted.event.session, &notification),
        })
    }
}

/// Wait for a notice's time to be up, or forever when there is no notice on
/// the row. Never resolving is what keeps the arm out of the way.
async fn expire(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Whether the session was still there when the paste finished.
enum Pasted {
    Done,
    Ended(Outcome),
}

/// Read this machine's clipboard and send what is on it to the session's
/// host, which writes it down and pastes the path.
///
/// The key goes through untouched when there is nothing to paste, so a
/// Ctrl-V that was meant for the program still reaches it. Everything else
/// is said on the status row: this runs while a full-screen program owns the
/// screen, and there is nowhere else to put a sentence.
async fn paste(
    reader: &mut SessionReader,
    writer: &mut SessionWriter,
    stdout: &mut Stdout,
    output: &mut Filter,
    status: &mut Status,
    takes_pastes: bool,
    key: &[u8],
) -> Result<Pasted> {
    let image = match clipboard::image().await {
        Ok(Some(image)) => image,
        // Text on the clipboard, or none at all. The ordinary case, and the
        // one that must stay silent: the key belongs to the session.
        Ok(None) => {
            writer.send_input(key).await?;
            return Ok(Pasted::Done);
        }
        // A missing helper program, or one that failed. Worth a sentence,
        // and the key still goes through.
        Err(e) => {
            status.set_notice(&format!("{e:#}"));
            writer.send_input(key).await?;
            return Ok(Pasted::Done);
        }
    };
    if !takes_pastes {
        status.set_notice("this host is too old to take pasted files; `mm update` there");
        writer.send_input(key).await?;
        return Ok(Pasted::Done);
    }

    let size = clipboard::mb(image.data.len());
    status.set_notice(&format!("pasting {size}"));
    if output.at_boundary() {
        stdout
            .write_all(status.repaint(terminal_size()).as_bytes())
            .await?;
        stdout.flush().await?;
    }

    // The screen has to stay alive while the bytes go: a session still
    // producing output would otherwise fill the connection nobody is
    // reading, and both ends would sit there waiting for the other. The
    // send is a single future polled to completion rather than one
    // recreated per pass, so nothing is ever cancelled mid-frame.
    let send = writer.send_paste(image.kind, &image.data);
    tokio::pin!(send);
    loop {
        tokio::select! {
            sent = &mut send => {
                sent?;
                status.set_notice(&format!("pasted {size}"));
                return Ok(Pasted::Done);
            }
            update = reader.next() => match update? {
                Update::Output(bytes) | Update::Screen(bytes) => {
                    stdout.write_all(&output.feed(&bytes)).await?;
                    stdout.flush().await?;
                }
                // Left unanswered: the writing half is busy with the paste,
                // and every chunk of it is a frame the host counts as this
                // client being alive.
                Update::Ping => {}
                // A bell during the second a paste takes. Dropped rather
                // than queued: this row is showing the paste, and a bell is
                // only worth anything while it is news.
                Update::Event(_) => {}
                // Unreachable: history comes at the start of an attach,
                // before there has been a key to press, and neither the
                // view, a search nor a rename is open while a paste is
                // running.
                Update::History(_)
                | Update::View(_)
                | Update::Found(_)
                | Update::Renamed(_)
                | Update::Resized(_) => {}
                Update::Exited(code) => return Ok(Pasted::Ended(Outcome::Exited(code))),
                Update::Disconnected => return Ok(Pasted::Ended(Outcome::Disconnected)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::picker::Row;
    use crate::settings::Screen;

    /// A window with no room for a box leaves the mark row to say what the
    /// popup would have: which list is open, and which row Enter would take.
    #[test]
    fn a_popup_with_no_room_to_draw_says_itself_in_one_line() {
        let rows = Rows {
            sessions: vec![Row::new(0, "build"), Row::new(1, "api")],
            groups: Vec::new(),
            at: 1,
        };
        assert_eq!(Popup::sessions(&rows).line(), "sessions: api");
    }

    /// An empty list still has to name itself. Saying nothing there would be a
    /// blank row in a mode that is still holding the keyboard, which is the
    /// thing this row exists to stop.
    #[test]
    fn a_list_with_nothing_in_it_still_names_itself() {
        assert_eq!(Popup::sessions(&Rows::default()).line(), "sessions: none");
    }

    /// The group list is about where you are looking, not about a session, so
    /// nothing that acts on a session may read a row of it as one. Its ids
    /// index the groups; handed back as a session, `n` there put whatever
    /// happened to sit at that index in the session list into the new group,
    /// on whatever machine it was on, and said nothing because the write
    /// worked.
    #[test]
    fn the_group_list_has_no_session_under_its_highlight() {
        let rows = Rows {
            sessions: vec![Row::new(0, "build"), Row::new(1, "api")],
            groups: vec![Row::new(0, "pi"), Row::new(1, "web")],
            at: 1,
        };
        let sessions = Popup::sessions(&rows);
        assert_eq!(sessions.subject(), Some(1), "the highlighted session");

        let narrowing = Popup {
            picker: Picker::new("groups", NARROW_HINTS, rows.groups.clone(), 0),
            what: Showing::Narrowing,
        };
        assert_eq!(narrowing.subject(), None);

        // Moving is the one that carries a session across from the other list,
        // which is why it holds the row rather than reading it off the picker.
        let moving = Popup {
            picker: Picker::new("move", MOVE_HINTS, rows.groups, 0),
            what: Showing::Moving { row: 1 },
        };
        assert_eq!(moving.subject(), Some(1));

        // And each list says which mode it is read in, because refusing a key
        // has to put the keyboard back where it came from: `KeyFilter::after`
        // has already moved it to `Mode::Rename` for a prompt that is not going
        // to open, and a mode with no prompt behind it falls through to the
        // session table, which over the group list is `m` reading a group row
        // as a session.
        assert_eq!(sessions.mode(), Mode::Control);
        assert_eq!(narrowing.mode(), Mode::Picking);
        assert_eq!(moving.mode(), Mode::Picking);
    }

    /// A mode the node turns back on for a session, and the client forgets to
    /// turn off, is left on in the shell. Focus reporting was the one that got
    /// noticed, because iTerm2 says so out loud; the mouse encodings would have
    /// been the next.
    #[cfg(feature = "desktop")]
    #[test]
    fn detaching_undoes_every_mode_a_reattach_replays() {
        let reset = reset(Screen::Alternate.mode(), false);
        for mode in crate::node::events::REPLAYED_MODES {
            assert!(
                reset.contains(&format!("\x1b[?{mode}l")),
                "detaching leaves private mode {mode} on"
            );
        }
        // And the ones avt's dump restores on attach, which are therefore not
        // on that list but are just as much ours to undo.
        for sequence in [
            "\x1b[?1l",    // normal cursor keys
            "\x1b[?6l",    // absolute addressing
            "\x1b[?7h",    // autowrap
            "\x1b[?25h",   // visible cursor
            "\x1b[?1047l", // alternate screen, both forms
            "\x1b[?1049l",
        ] {
            assert!(
                reset.contains(sequence),
                "detaching leaves {sequence:?} unsent"
            );
        }
    }

    /// The keyboard protocols are the client's to undo for the same reason the
    /// private modes are: the program that asked for one is still running on a
    /// terminal that is no longer this one, and a shell handed back a keyboard
    /// still in that mode reads escape sequences where it expects keys.
    #[cfg(feature = "desktop")]
    #[test]
    fn detaching_undoes_the_keyboard_protocols_too() {
        let reset = reset(Screen::Alternate.mode(), false);
        for sequence in [
            "\x1b[<16u",  // pop kitty's stack, however deep the program went
            "\x1b[=0;1u", // and clear flags it set without pushing
            "\x1b[>4;0m", // xterm's modifyOtherKeys
        ] {
            assert!(
                reset.contains(sequence),
                "detaching leaves {sequence:?} unsent"
            );
        }
    }

    /// The bug this was written for: hopping to a session smaller or emptier
    /// than the one left behind showed the two mixed, because the screen dump
    /// paints down to its last line with anything on it and no further.
    #[cfg(feature = "desktop")]
    #[test]
    fn a_hop_erases_the_session_before_it() {
        let takeover = takeover(Screen::Alternate.mode(), false);
        let erase = takeover.find("\x1b[2J").expect("a hop erases nothing");
        assert!(
            takeover[..erase].contains("\x1b[H"),
            "a hop erases without homing, so the dump starts where the last session's cursor was"
        );
        assert!(
            takeover[..erase].contains("\x1b[0m"),
            "the erase runs with a pen the last session set, and paints the screen its colour"
        );
    }

    /// The same pair as a detach: a mode switched on for the session you leave
    /// is a mode left on in the session you land in, which never asked for it.
    #[cfg(feature = "desktop")]
    #[test]
    fn a_hop_undoes_every_mode_the_session_before_it_switched_on() {
        let takeover = takeover(Screen::Alternate.mode(), false);
        for mode in crate::node::events::REPLAYED_MODES {
            assert!(
                takeover.contains(&format!("\x1b[?{mode}l")),
                "hopping leaves private mode {mode} on"
            );
        }
        for sequence in ["\x1b[<16u", "\x1b[=0;1u", "\x1b[>4;0m", "\x1b[0 q"] {
            assert!(
                takeover.contains(sequence),
                "hopping leaves {sequence:?} unsent"
            );
        }
    }

    /// A screen the client asked for is painted over a terminal put back to
    /// nothing, because the replay riding with it says what the program has on
    /// and can say nothing about what it turned off. Without this, a program
    /// that popped the keyboard protocol while the popup was up left the
    /// terminal reporting key releases at the shell that followed it.
    #[cfg(feature = "desktop")]
    #[test]
    fn a_screen_the_client_asked_for_is_painted_over_the_modes_switched_off() {
        let off = given_back();
        for mode in crate::node::events::REPLAYED_MODES {
            assert!(
                off.contains(&format!("\x1b[?{mode}l")),
                "a dump would replay private mode {mode} onto one already on"
            );
        }
        for sequence in ["\x1b[<16u", "\x1b[=0;1u", "\x1b[>4;0m", "\x1b[0 q"] {
            assert!(off.contains(sequence), "{sequence:?} is never sent");
        }
        // The pair the whole thing rests on: a hop undoes the same set, so
        // neither can grow a member the other has not heard of.
        assert!(undone().contains(&off));
    }

    /// And the reason that set cannot be written on its own: the client's hold
    /// on the mouse is spelt in the same modes a session asks for, so a screen
    /// painted while it held the wheel handed the wheel back without saying so.
    /// Everything after the first popup then scrolled the terminal instead.
    #[cfg(feature = "desktop")]
    #[test]
    fn switching_the_modes_off_switches_the_clients_own_mouse_off_too() {
        let off = given_back();
        assert!(
            WHEEL_OURS
                .split("\x1b[")
                .filter(|mode| !mode.is_empty())
                .all(|mode| off.contains(&format!("\x1b[{}l", mode.trim_end_matches('h')))),
            "the wheel would survive `given_back` and need no re-asserting"
        );
    }

    /// A hop is a detach for the session being left, but not for the terminal:
    /// the alternate screen and the pushed title belong to the run of attaches,
    /// not to one of them. Giving either back here would drop the client onto
    /// the shell's screen on every switch.
    #[cfg(feature = "desktop")]
    #[test]
    fn a_hop_stays_on_the_alternate_screen() {
        let takeover = takeover(Screen::Alternate.mode(), false);
        for sequence in ["\x1b[?1049l", "\x1b[?1047l", "\x1b[23;2t"] {
            assert!(
                !takeover.contains(sequence),
                "hopping sends {sequence:?}, which gives the terminal back mid-attach"
            );
        }
    }
}
