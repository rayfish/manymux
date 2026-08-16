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
use super::{Action, Find, KeyFilter, Mode, Outcome, Rename, Scroll};
use crate::client::screen::ScreenMode;
use crate::client::scroll::Scrollback;
use crate::client::status::{self, Filter, Status};
use crate::client::{Attached, SessionHalves, SessionReader, SessionWriter, Update};
use crate::clipboard;
use crate::notify;
use crate::proto::{HostedEvent, Renamed, Size};
use crate::settings::Screen;

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
    use std::fmt::Write as _;

    let mut undone = String::new();
    // Both of these home the cursor, so they go first, while the alternate
    // screen is still up and the cursor there is about to be discarded.
    // Afterwards they would drop the shell's prompt in the top-left corner.
    undone.push_str("\x1b[r"); // full-height scrolling region
    undone.push_str("\x1b[?6l"); // absolute cursor addressing, not origin mode

    undone.push_str("\x1b[?25h"); // show the cursor
    undone.push_str("\x1b[0 q"); // the cursor shape this terminal defaults to
    undone.push_str("\x1b[0m"); // default attributes
    undone.push_str("\x1b[?7h"); // autowrap on
    undone.push_str("\x1b[4l"); // replace mode, not insert
    undone.push_str("\x1b[?1l\x1b>"); // normal cursor keys and keypad
    for mode in crate::node::events::REPLAYED_MODES {
        let _ = write!(undone, "\x1b[?{mode}l");
    }

    // The extended-keys protocols, off the same way. A program that asked
    // for one and was left running is asking the terminal it is no longer
    // on, and a shell handed one back still in that mode reads `\x1b[13;2u`
    // where it expects a carriage return.
    //
    // The count is kitty's whole stack, because the pushing was the
    // program's and there is no telling how deep it went; popping past the
    // bottom does nothing. The set that follows is for a program that
    // changed the flags without pushing, which the pops cannot undo.
    undone.push_str("\x1b[<16u\x1b[=0;1u");
    undone.push_str("\x1b[>4;0m"); // and xterm's older modifyOtherKeys
    undone
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
/// The keyboard is still read, because a wait nobody can leave is worse than
/// no wait at all. The mode key's detach is one way out, so that leaving a
/// session that is gone is the same gesture as leaving one that is there. The
/// other is Ctrl-C, which has nowhere else to go while there is no session to
/// send it to, and which is what a hand reaches for anyway.
pub async fn waiting(held: &mut Held, target: &str, delay: Duration) -> Wait {
    let mut status = Status::new(target);
    status.set_notice(&format!(
        "{target} is not answering, reconnecting in {}s",
        delay.as_secs().max(1)
    ));
    write_now(&status.repaint(terminal_size()));

    let mut keys = KeyFilter::new(crate::client::attach::prefix());
    let until = tokio::time::sleep(delay);
    tokio::pin!(until);
    loop {
        tokio::select! {
            () = &mut until => return Wait::Retry,
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

/// Ctrl-C, which while nothing is attached is a way out rather than a signal
/// to pass on.
const INTERRUPT: u8 = 0x03;

/// Run the attach loop until the client detaches, switches away, or the
/// session ends.
///
/// `mode` is where the keyboard starts, which is how a hop carries control
/// mode through the reattach.
/// The name the session answers to at the end comes back with the outcome:
/// a rename from inside the session changes what the caller has to call it,
/// and this half of the client has no way to tell it any other way.
pub async fn run(
    held: &mut Held,
    session: Attached,
    target: &str,
    mode: Mode,
) -> Result<(Outcome, Option<String>)> {
    let mut status = Status::new(target);
    status.set_mode(mode);
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
        screen,
        on_alternate,
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

/// Take the wheel, or give it back.
///
/// When that is, and why it is so rarely, is [`wheel_is_ours`]. The arrow
/// keys a terminal would make of a notch with nobody reporting are not this
/// function's problem: alternate scroll is off for the whole run of
/// attaches, in [`crate::client::screen`].
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
    let sequence = if ours {
        "\x1b[?1000h\x1b[?1006h" // report buttons, in the SGR spelling
    } else {
        // Off in the other order, so nothing is left reporting in a
        // spelling that is no longer switched on.
        "\x1b[?1006l\x1b[?1000l"
    };
    stdout.write_all(sequence.as_bytes()).await?;
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

/// The name in `naming` is written back when a rename lands: the mark row
/// is not the only thing that has to follow it.
async fn pump(
    keyboard: &mut mpsc::Receiver<Vec<u8>>,
    session: Attached,
    mut status: Status,
    mode: Mode,
    naming: Naming<'_>,
    screen: Screen,
    on_alternate: Arc<AtomicBool>,
) -> Result<Outcome> {
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
    let mut output = Filter::new(screen);
    // The view over the session's history, while it is up.
    let mut scrolling: Option<Scrollback> = None;
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
    // Screens still owed for a resize, and so still to be painted onto one
    // wiped first. Counted rather than flagged because a drag across the
    // desktop asks more than once before the first answer arrives.
    let mut owed = 0usize;
    // When the notice on the row stops being worth showing.
    let mut notice_until: Option<tokio::time::Instant> = None;
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
                let keystrokes = keys.filter(&typed);
                if !keystrokes.forward.is_empty() {
                    writer.send_input(&keystrokes.forward).await?;
                }
                if keystrokes.mode != status.mode() {
                    status.set_mode(keystrokes.mode);
                    restate = true;
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
                        writer.resync().await?;
                        owed += 1;
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
                            .get_or_insert_with(|| Scrollback::new(terminal_size()));
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
                            .get_or_insert_with(|| Scrollback::new(terminal_size()));
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
                    // The same shape as a search, and the same reason for
                    // it: the name lives in the key filter until Enter, so
                    // all there is to do until then is keep the row saying
                    // what is in it.
                    Some(Action::Rename(_)) if !renames => {
                        keys.stop_typing();
                        keys.set_mode(Mode::Focus);
                        status.set_mode(Mode::Focus);
                        status.set_renaming(None);
                        status.set_notice(
                            "this host is too old to rename from here; `mm rename` instead",
                        );
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
                                // Nothing is said here: the row keeps
                                // naming the session what it is still
                                // called until the host says otherwise.
                                writer.rename(&wanted).await?;
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
                            writer.resync().await?;
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
                // The view has just opened or just closed, and the wheel
                // goes with it: the terminal has the mouse back the moment
                // there is a live session to select on.
                let ours = wheel_is_ours(scrolling.is_some(), output.session_mouse());
                own_the_wheel(&mut stdout, &mut keys, &mut wheel, ours).await?;
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
                    // step with the byte stream, but not written while the
                    // view is up: the screen is showing the history, and
                    // the session painting over it is what leaving the view
                    // asks the node to undo.
                    if scrolling.is_none() {
                        stdout.write_all(&bytes).await?;
                    }
                    on_alternate.store(output.on_alternate(), Ordering::Relaxed);
                    // The session may have just asked for the mouse, or
                    // given it back. Only between sequences, like the mark.
                    if output.at_boundary() {
                        let ours = wheel_is_ours(scrolling.is_some(), output.session_mouse());
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
                        writer.resync().await?;
                    }
                    painted = true;
                    // Only between sequences: a repaint written into the
                    // middle of one would corrupt it. Whatever cleared the
                    // mark stays noted until there is a safe moment.
                    if output.at_boundary() {
                        restate |= output.take_dirty();
                    }
                    settle(&mut stdout, &output, &status, &mut pending, &mut restate).await?;
                }
                // The screen we asked for. Its own switches are how a dump
                // paints both buffers, so they are swallowed and dropped
                // rather than answered with another request.
                Update::Screen(bytes) => {
                    if owed > 0 {
                        owed -= 1;
                        stdout.write_all(REGROWN.as_bytes()).await?;
                    }
                    stdout.write_all(&output.feed(&bytes)).await?;
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
                    if let Some(view) = scrolling.as_mut() {
                        view.take(window);
                        status.set_scrolled(Some(view.offset()));
                        stdout.write_all(view.paint().as_bytes()).await?;
                        stdout
                            .write_all(status.repaint(terminal_size()).as_bytes())
                            .await?;
                        stdout.flush().await?;
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
                    view.resize(size);
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
                writer.resync().await?;
                owed += 1;
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
                | Update::Renamed(_) => {}
                Update::Exited(code) => return Ok(Pasted::Ended(Outcome::Exited(code))),
                Update::Disconnected => return Ok(Pasted::Ended(Outcome::Disconnected)),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Screen;

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
