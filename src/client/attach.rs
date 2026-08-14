//! Driving a real terminal from an attached session.
//!
//! The terminal-specific half of the client. A mobile app skips this entirely
//! and drives [`crate::client::Attached`] directly, feeding the bytes to its
//! own terminal widget.
//!
//! The client stays deliberately dumb: raw mode, forward keystrokes, paint what
//! arrives, watch for the detach key. All the state lives on the server, which
//! is what makes detaching free.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::client::{Attached, SessionHalves, Update};

/// The key that goes from focus mode to control mode: Ctrl-] (0x1d).
///
/// Not tmux's Ctrl-b or screen's Ctrl-a, because you are quite likely running
/// one of those *inside* a manymux session: manymux has no panes or tabs, so
/// splitting a window is still their job, and taking their prefix would mean
/// swallowing it before it ever reached them.
///
/// Not Ctrl-Space either, which macOS takes for switching input sources, and
/// fcitx5 and ibus take on Linux. And not Ctrl-` despite the arithmetic saying
/// it is the same NUL: a terminal only masks the top bits off `@`, `A`-`Z`,
/// `[`, `\`, `]`, `^`, `_` and space, and the backtick is outside that set, so
/// what arrives is a plain backtick that no client could tell from a typed one.
///
/// `]` is in the set, every terminal sends it unasked, and what wants it back
/// is vim's jump-to-tag and telnet's escape. Pressing the key twice quickly
/// sends one through, which covers both.
pub const DEFAULT_PREFIX: u8 = 0x1d;

/// The key in force, from `MM_PREFIX` if it is set and usable.
///
/// Accepts `C-]`, `C-b`, `^B`, `C-Space` or `\x02`. An unusable value is a
/// warning rather than a failure: losing the ability to detach because of a
/// typo in an environment variable would be worse than ignoring it.
pub fn prefix() -> u8 {
    let Some(text) = std::env::var_os("MM_PREFIX") else {
        return DEFAULT_PREFIX;
    };
    let text = text.to_string_lossy();
    match parse_prefix(&text) {
        Some(byte) => byte,
        None => {
            eprintln!("mm: MM_PREFIX={text:?} is not a control key; using Ctrl-]");
            DEFAULT_PREFIX
        }
    }
}

/// Parse a control key: `C-]`, `C-b`, `c-B`, `^b`, a bare `b`, `C-Space`, or
/// the raw byte.
///
/// A bare letter is read as the control key, since a printable character could
/// not serve as the key anyway: it would take you out of the session on every
/// one you typed.
fn parse_prefix(text: &str) -> Option<u8> {
    let key = text
        .strip_prefix("C-")
        .or_else(|| text.strip_prefix("c-"))
        .or_else(|| text.strip_prefix('^'))
        .unwrap_or(text);

    // Spelled out, because `MM_PREFIX=C- ` is not something anyone would type.
    if key.eq_ignore_ascii_case("space") {
        return Some(0x00);
    }

    let mut chars = key.chars();
    let key = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    // A control character given literally, `$'\x02'` style.
    if key.is_control() {
        return Some(key as u8);
    }
    // `C-b` is 0x02: the letter with the top three bits cleared. The same
    // arithmetic covers `C-\`, `C-]` and friends just past `Z`. The backtick
    // past those is here for a terminal configured to send NUL for it, not
    // because one does on its own.
    let byte = u8::try_from(key).ok()?.to_ascii_uppercase();
    (0x40..=0x60).contains(&byte).then_some(byte & 0x1f)
}

/// The key that pastes what is on this machine's clipboard: Ctrl-V (0x16).
///
/// The key everything else already uses for it, and the one `claude` itself
/// listens for. Taken from the session only when the clipboard actually holds
/// an image: with text on it, or nothing, the byte goes through and vim's
/// visual block still works. `MM_PASTE=off` gives the key back entirely.
pub const PASTE_KEY: u8 = 0x16;

/// Which way a switch key moves through the sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Next,
    Previous,
    /// Back to the one you came from.
    Last,
}

/// What a key pressed in switch mode asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Detach,
    Switch(Motion),
    /// Send this machine's clipboard to the session, if there is an image on
    /// it. Deciding that is the caller's: this half of the client knows nothing
    /// about clipboards.
    Paste,
}

/// How the attach ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Detached,
    /// A switch key was pressed. Which session it lands on is the caller's to
    /// work out: this half of the client knows nothing about hosts.
    Switch(Motion),
    /// The session's process exited with this code.
    Exited(i32),
    /// The host went away.
    Disconnected,
}

/// Which of the client's two modes the keyboard is in.
///
/// Modal like vim, and for the same reason: the keys that drive the client are
/// the ones a session wants for itself, so rather than reserving a chord for
/// each of them, one key moves between a mode where everything you type is the
/// session's and a mode where the keys are the client's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every keystroke goes to the session. Where you spend your time.
    #[default]
    Focus,
    /// The keys drive the client: switch sessions, detach, back to focus.
    Control,
}

impl Mode {
    /// What the bottom row calls it.
    pub fn name(self) -> &'static str {
        match self {
            Mode::Focus => "focus",
            Mode::Control => "control",
        }
    }
}

/// Watches the keystroke stream for the key that changes mode, and reads the
/// keys that follow it.
///
/// Control mode stays on: one mode key then `tab tab tab` walks through the
/// sessions. `Esc`, `Enter` or the mode key goes back to focus, `d` detaches,
/// and the mode key hit twice in a row quickly also sends one through for
/// whatever wants it inside the session. Any other key drops back to focus and
/// passes both bytes through unchanged, so a mistyped mode key costs you
/// visible junk rather than a silently swallowed line.
pub struct KeyFilter {
    prefix: u8,
    /// Whether [`PASTE_KEY`] is the client's or the session's.
    paste: bool,
    mode: Mode,
    /// When the key that turned control mode on was pressed, while it is still
    /// the last key pressed.
    ///
    /// It matters for exactly one key: its own. The key always goes back to
    /// focus, and this decides whether a literal one goes to the session on the
    /// way out. Two in a row inside [`LITERAL`] are the sequence that means the
    /// byte, so one is sent. Anything slower is a hand that went in and came
    /// out again, so nothing is: a mode you sat in for a while, or one a switch
    /// left on, was never a request for that byte.
    pressed: Option<Instant>,
}

/// How long after the mode key a second one still means "send me the byte"
/// rather than "back to focus". Long enough not to need a fast hand, short
/// enough that nothing you left the mode sitting in counts.
const LITERAL: Duration = Duration::from_secs(3);

impl Default for KeyFilter {
    fn default() -> Self {
        Self {
            paste: paste_enabled(),
            ..Self::new(prefix())
        }
    }
}

/// Whether the paste key is watched for by default. A client with no clipboard
/// of its own to read (a phone, which has its own way of sending one) is left
/// to decide for itself.
#[cfg(feature = "desktop")]
fn paste_enabled() -> bool {
    crate::clipboard::enabled()
}

#[cfg(not(feature = "desktop"))]
fn paste_enabled() -> bool {
    true
}

/// What a chunk of keystrokes amounts to once the client's own keys are taken
/// out.
#[derive(Debug, PartialEq, Eq)]
pub struct Keystrokes {
    /// The bytes to send on to the session.
    pub forward: Vec<u8>,
    /// What the user asked for, if anything.
    pub action: Option<Action>,
    /// The mode the client is in now, for the row at the bottom of the screen.
    pub mode: Mode,
}

impl KeyFilter {
    pub fn new(prefix: u8) -> Self {
        Self {
            prefix,
            paste: true,
            mode: Mode::Focus,
            pressed: None,
        }
    }

    /// Whether the paste key is the client's. Off hands it back to the session,
    /// which is what `MM_PASTE=off` asks for.
    pub fn set_paste(&mut self, on: bool) {
        self.paste = on;
    }

    /// Start in a mode. Attaching after a switch starts in control mode, which
    /// is what makes `tab tab tab` walk the list across the hops.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.pressed = None;
    }

    pub fn filter(&mut self, input: &[u8]) -> Keystrokes {
        self.filter_at(input, Instant::now())
    }

    /// [`Self::filter`] with the clock handed in, so the window a literal mode
    /// key lives in can be tested without waiting out three real seconds.
    fn filter_at(&mut self, input: &[u8], now: Instant) -> Keystrokes {
        let mut forward = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            i += 1;
            if self.mode == Mode::Focus {
                if b == self.prefix {
                    self.mode = Mode::Control;
                    self.pressed = Some(now);
                } else if self.paste && b == PASTE_KEY {
                    // Handed up rather than swallowed: the caller sends the
                    // key on when the clipboard turns out to hold nothing to
                    // paste, so the session keeps the key on every press that
                    // was not one.
                    return Keystrokes {
                        forward,
                        action: Some(Action::Paste),
                        mode: self.mode,
                    };
                } else {
                    forward.push(b);
                }
                continue;
            }
            let pressed = self.pressed.take();
            let action = match b {
                // First, so that a mode key which is itself one of the keys
                // below can still be sent through by pressing it twice.
                b if b == self.prefix => {
                    if pressed.is_some_and(|at| now.duration_since(at) < LITERAL) {
                        forward.push(self.prefix);
                    }
                    self.mode = Mode::Focus;
                    None
                }
                b'd' | b'D' => Some(Action::Detach),
                b'\t' | b'n' | b'N' => Some(Action::Switch(Motion::Next)),
                b'p' | b'P' => Some(Action::Switch(Motion::Previous)),
                b'l' | b'L' => Some(Action::Switch(Motion::Last)),
                // Shift-Tab, which starts with the same byte as the Esc that
                // goes back to focus. An Esc with `[Z` behind it in the same
                // read is the key; one at the end of a read is a real Esc.
                // Split across two reads it reads as an Esc, which costs a trip
                // back to focus and nothing else.
                0x1b if input[i..].starts_with(b"[Z") => {
                    i += 2;
                    Some(Action::Switch(Motion::Previous))
                }
                0x1b | b'\r' | b'\n' => {
                    self.mode = Mode::Focus;
                    None
                }
                other => {
                    self.mode = Mode::Focus;
                    forward.push(self.prefix);
                    forward.push(other);
                    None
                }
            };
            // Whatever is left of the chunk is dropped: nobody types through a
            // detach or a switch.
            if let Some(action) = action {
                // A switch leaves control mode on, so the next key carries on
                // walking without a mode key of its own.
                self.mode = match action {
                    Action::Detach | Action::Paste => Mode::Focus,
                    Action::Switch(_) => Mode::Control,
                };
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                };
            }
        }
        Keystrokes {
            forward,
            action: None,
            mode: self.mode,
        }
    }
}

#[cfg(feature = "desktop")]
pub use terminal::{Held, hold, run, session_size, terminal_size};

#[cfg(feature = "desktop")]
mod terminal {
    use std::io::IsTerminal;
    use std::time::Duration;

    use anyhow::{Result, bail};
    use crossterm::terminal;
    use tokio::io::{AsyncWriteExt, Stdout};
    use tokio::signal::unix::{SignalKind, signal};
    use tokio::sync::mpsc;

    use super::{Action, KeyFilter, Mode, Outcome, PASTE_KEY};
    use crate::client::status::{self, Filter, Status};
    use crate::client::{Attached, SessionHalves, SessionReader, SessionWriter, Update};
    use crate::clipboard;
    use crate::proto::Size;

    /// Sent before attaching.
    ///
    /// The alternate screen is the important part. A session's repaint places
    /// everything by absolute coordinates, because that is what a screen dump
    /// is, so painting it onto your shell's screen writes the session over your
    /// scrollback and leaves the cursor at the session's row 1 rather than
    /// where the session's prompt appears to be. Anything you then type lands
    /// at the top of the terminal. On a surface of its own the coordinates mean
    /// what they say, and detaching gives your shell's screen back untouched.
    ///
    /// The title is pushed for the same reason: detaching should give you back
    /// the tab name you had, not leave it named after a session you left.
    const SETUP: &str = concat!(
        "\x1b[22;2t",  // push the window title
        "\x1b[?1049h", // switch to the alternate screen
    );

    /// Terminal state a full-screen program may have left behind, undone when
    /// we give the terminal back, so a detach never leaves your shell with an
    /// invisible cursor, a stuck mouse mode, or focus reporting still on.
    ///
    /// The private modes come from the same list a reattach replays: whatever
    /// the node switches back on for the session is exactly what has to be
    /// switched off again here, or it leaks into the shell you return to.
    pub(super) fn reset() -> String {
        use std::fmt::Write as _;

        let mut reset = String::new();
        // Both of these home the cursor, so they go first, while the alternate
        // screen is still up and the cursor there is about to be discarded.
        // Afterwards they would drop the shell's prompt in the top-left corner.
        reset.push_str("\x1b[r"); // full-height scrolling region
        reset.push_str("\x1b[?6l"); // absolute cursor addressing, not origin mode

        reset.push_str("\x1b[23;2t"); // pop the title pushed on attach
        reset.push_str("\x1b[?1047l"); // leave the alternate screen (the older form, which vim uses)
        reset.push_str("\x1b[?1049l"); // leave the alternate screen

        reset.push_str("\x1b[?25h"); // show the cursor
        reset.push_str("\x1b[0 q"); // the cursor shape this terminal defaults to
        reset.push_str("\x1b[0m"); // default attributes
        reset.push_str("\x1b[?7h"); // autowrap on
        reset.push_str("\x1b[4l"); // replace mode, not insert
        reset.push_str("\x1b[?1l\x1b>"); // normal cursor keys and keypad
        for mode in crate::node::events::REPLAYED_MODES {
            let _ = write!(reset, "\x1b[?{mode}l");
        }

        // Column zero, but no newline: leaving the alternate screen already put
        // the cursor back where the shell left it, on the line after the command
        // you typed. A newline here would print the detach message one blank
        // line further down for no reason.
        reset.push('\r');
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
    /// does not flap the alternate screen between every hop. Dropping it is
    /// what restores the terminal, on the error paths too.
    pub struct Held {
        _private: (),
    }

    pub fn hold() -> Result<Held> {
        if !std::io::stdin().is_terminal() {
            bail!("attach needs a terminal on stdin");
        }
        // A panic prints to stderr, which while this is held means printing
        // onto the alternate screen, which the unwind then throws away. Give
        // the terminal back first, so whatever went wrong is readable on the
        // screen the shell gets back.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic| {
            let _ = terminal::disable_raw_mode();
            write_now(&reset());
            previous(panic);
        }));
        terminal::enable_raw_mode()?;
        write_now(SETUP);
        Ok(Held { _private: () })
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let _ = terminal::disable_raw_mode();
            write_now(&reset());
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

    /// Run the attach loop until the client detaches, switches away, or the
    /// session ends.
    ///
    /// `mode` is where the keyboard starts, which is how a hop carries control
    /// mode through the reattach.
    pub async fn run(_held: &Held, session: Attached, target: &str, mode: Mode) -> Result<Outcome> {
        let mut status = Status::new(target);
        status.set_mode(mode);
        write_now(&status.setup(terminal_size()));
        pump(session, status, mode).await
    }

    /// How long a notice from the client itself stays on the row before the key
    /// hints have it back. Long enough to read without looking for it.
    const NOTICE_FOR: Duration = Duration::from_secs(5);

    /// Chunks of keystrokes, read on a thread of its own.
    ///
    /// `tokio::io::stdin` would do the same reads, but on a blocking pool
    /// thread, and a read on one of those cannot be cancelled: the runtime
    /// waits for it before it will shut down. Nobody types at a client whose
    /// session has just ended, so that wait is forever, and the process hangs
    /// on with the terminal already given back until a keystroke happens to
    /// land. A thread nobody joins reads the same bytes and holds up nothing.
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

    async fn pump(session: Attached, mut status: Status, mode: Mode) -> Result<Outcome> {
        let takes_pastes = session.paste;
        let SessionHalves {
            mut reader,
            mut writer,
        } = session.split();
        let mut keyboard = keyboard();
        let mut stdout = tokio::io::stdout();
        let mut winch = signal(SignalKind::window_change())?;
        let mut keys = KeyFilter::default();
        keys.set_mode(mode);
        let mut output = Filter::default();
        // The row no longer says what the keys are doing. Held until there is a
        // safe moment to draw, the same as a mark the session cleared.
        let mut restate = false;
        // Whether the frame that repaints the screen on attach has been and
        // gone. Everything after it is the session speaking for itself.
        let mut painted = false;
        // When the notice on the row stops being worth showing.
        let mut notice_until: Option<tokio::time::Instant> = None;

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
                    if restate && output.at_boundary() {
                        stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                        stdout.flush().await?;
                        restate = false;
                    }
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
                        Some(Action::Paste) => {
                            let pasted = paste(
                                &mut reader,
                                &mut writer,
                                &mut stdout,
                                &mut output,
                                &mut status,
                                takes_pastes,
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
                }
                update = reader.next() => match update? {
                    Update::Output(bytes) => {
                        stdout.write_all(&output.feed(&bytes)).await?;
                        // A screen switch went no further than this client, so
                        // the terminal is still showing the screen the session
                        // has just left. Ask for the other one, which exists
                        // only in the node's model of the session. The frame
                        // that repaints on attach is a dump like the answer
                        // would be, and asking for another of those is a round
                        // trip that paints the same screen twice.
                        if output.take_switched() && painted {
                            writer.resync().await?;
                        }
                        painted = true;
                        // Only between sequences: a repaint written into the
                        // middle of one would corrupt it. Whatever cleared the
                        // mark stays noted until there is a safe moment.
                        if output.at_boundary() && (output.take_dirty() || restate) {
                            stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                            restate = false;
                        }
                        stdout.flush().await?;
                    }
                    // The screen we asked for. Its own switches are how a dump
                    // paints both buffers, so they are swallowed and dropped
                    // rather than answered with another request.
                    Update::Screen(bytes) => {
                        stdout.write_all(&output.feed(&bytes)).await?;
                        output.take_switched();
                        output.take_dirty();
                        // The dump put the session's own screen back, mark and
                        // region included, so both are ours to draw again.
                        restate = true;
                        if output.at_boundary() {
                            stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                            restate = false;
                        }
                        stdout.flush().await?;
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
                    // The new geometry moved the mark and the region with it.
                    stdout.write_all(status.repaint(size).as_bytes()).await?;
                    stdout.flush().await?;
                }
                // A notice the client put on the row has been up long enough.
                _ = expire(notice_until) => {
                    notice_until = None;
                    status.clear_notice();
                    restate = true;
                    if output.at_boundary() {
                        stdout.write_all(status.repaint(terminal_size()).as_bytes()).await?;
                        stdout.flush().await?;
                        restate = false;
                    }
                }
            }
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
    ) -> Result<Pasted> {
        let image = match clipboard::image().await {
            Ok(Some(image)) => image,
            // Text on the clipboard, or none at all. The ordinary case, and the
            // one that must stay silent: the key belongs to the session.
            Ok(None) => {
                writer.send_input(&[PASTE_KEY]).await?;
                return Ok(Pasted::Done);
            }
            // A missing helper program, or one that failed. Worth a sentence,
            // and the key still goes through.
            Err(e) => {
                status.set_notice(&format!("{e:#}"));
                writer.send_input(&[PASTE_KEY]).await?;
                return Ok(Pasted::Done);
            }
        };
        if !takes_pastes {
            status.set_notice("this host is too old to take pasted files; `mm update` there");
            writer.send_input(&[PASTE_KEY]).await?;
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
                    Update::Exited(code) => return Ok(Pasted::Ended(Outcome::Exited(code))),
                    Update::Disconnected => return Ok(Pasted::Ended(Outcome::Disconnected)),
                },
            }
        }
    }
}

/// Everything an attached session produced before it stopped.
#[derive(Debug)]
pub struct Collected {
    pub output: Vec<u8>,
    pub outcome: Outcome,
}

/// Drive an attached session without a terminal, for tests and for clients that
/// render the output themselves.
pub async fn collect_until(
    session: Attached,
    mut stop: impl FnMut(&[u8]) -> bool,
) -> Result<Collected> {
    let SessionHalves { mut reader, .. } = session.split();
    let mut output = Vec::new();
    loop {
        let outcome = match reader.next().await? {
            Update::Output(bytes) | Update::Screen(bytes) => {
                output.extend_from_slice(&bytes);
                if stop(&output) {
                    Outcome::Detached
                } else {
                    continue;
                }
            }
            // Nothing here to answer with, the writing half having been
            // dropped. Never answering is what leaves the host holding this
            // client to no deadline, which is what a caller collecting output
            // without a connection to keep alive wants.
            Update::Ping => continue,
            Update::Exited(code) => Outcome::Exited(code),
            Update::Disconnected => Outcome::Disconnected,
        };
        return Ok(Collected { output, outcome });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keystrokes that went straight through, leaving the keyboard in focus.
    fn forwarded(bytes: &[u8]) -> Keystrokes {
        Keystrokes {
            forward: bytes.to_vec(),
            action: None,
            mode: Mode::Focus,
        }
    }

    /// A key that asked for something, with nothing forwarded alongside it.
    fn asked(action: Action, mode: Mode) -> Keystrokes {
        Keystrokes {
            forward: vec![],
            action: Some(action),
            mode,
        }
    }

    /// The mode key, whatever it is. Named because a control byte in the middle
    /// of a byte string is unreadable.
    const KEY: u8 = DEFAULT_PREFIX;

    /// A mode the node turns back on for a session, and the client forgets to
    /// turn off, is left on in the shell. Focus reporting was the one that got
    /// noticed, because iTerm2 says so out loud; the mouse encodings would have
    /// been the next.
    #[cfg(feature = "desktop")]
    #[test]
    fn detaching_undoes_every_mode_a_reattach_replays() {
        let reset = terminal::reset();
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

    #[test]
    fn a_control_key_can_be_named_several_ways() {
        assert_eq!(parse_prefix("C-b"), Some(0x02));
        assert_eq!(parse_prefix("c-B"), Some(0x02));
        assert_eq!(parse_prefix("^b"), Some(0x02));
        assert_eq!(parse_prefix("\u{2}"), Some(0x02));
        // The keys past `Z` that people pick.
        assert_eq!(parse_prefix("C-a"), Some(0x01));
        assert_eq!(parse_prefix("C-\\"), Some(0x1c));
    }

    #[test]
    fn every_spelling_of_the_default_key_is_the_same_byte() {
        assert_eq!(parse_prefix("C-]"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("c-]"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("^]"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("]"), Some(DEFAULT_PREFIX));
        // And the byte itself, however it got into the variable.
        assert_eq!(parse_prefix("\u{1d}"), Some(DEFAULT_PREFIX));
    }

    #[test]
    fn the_keys_a_terminal_masks_to_nul_are_one_key() {
        // A terminal clears the top bits off `@` and space alike, so both
        // arrive as a NUL and `MM_PREFIX` cannot tell them apart. The backtick
        // is not one a terminal masks, but it parses for anyone who has bound
        // it to send the byte.
        assert_eq!(parse_prefix("C-@"), Some(0x00));
        assert_eq!(parse_prefix("C-Space"), Some(0x00));
        assert_eq!(parse_prefix("c-space"), Some(0x00));
        assert_eq!(parse_prefix("^Space"), Some(0x00));
        assert_eq!(parse_prefix("C-`"), Some(0x00));
        assert_eq!(parse_prefix("\0"), Some(0x00));
    }

    #[test]
    fn a_bare_letter_means_the_control_key() {
        // The only reading that works: a printable key would take you out of
        // the session on every one of those characters you typed.
        assert_eq!(parse_prefix("b"), Some(0x02));
    }

    #[test]
    fn a_key_that_is_not_a_key_at_all_is_refused() {
        // Refused rather than silently mangled, and the caller then warns and
        // keeps the default, since a typo here must not cost you the ability
        // to detach.
        assert_eq!(parse_prefix("C-bb"), None);
        assert_eq!(parse_prefix(""), None);
        assert_eq!(parse_prefix("C-"), None);
        assert_eq!(parse_prefix("1"), None);
    }

    #[test]
    fn the_mode_key_can_be_tmuxs() {
        // `MM_PREFIX=C-b` for muscle memory, at the price of tmux inside a
        // session no longer seeing its own prefix.
        let mut f = KeyFilter::new(0x02);
        assert_eq!(
            f.filter(b"ls\x02d"),
            Keystrokes {
                forward: b"ls".to_vec(),
                action: Some(Action::Detach),
                mode: Mode::Focus,
            }
        );
        // And the default key is then just an ordinary keystroke again.
        let mut f = KeyFilter::new(0x02);
        assert_eq!(f.filter(&[KEY]), forwarded(&[KEY]));
    }

    #[test]
    fn the_paste_key_is_handed_up_rather_than_swallowed() {
        // Handed up, because whether it is the client's key at all depends on
        // what is on the clipboard, and the filter has no way to know. What is
        // typed before it still goes to the session.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[b'l', b's', PASTE_KEY]),
            Keystrokes {
                forward: b"ls".to_vec(),
                action: Some(Action::Paste),
                mode: Mode::Focus,
            }
        );
        // And the keyboard is still in focus afterwards: pasting is not a mode.
        assert_eq!(f.filter(b"x"), forwarded(b"x"));
    }

    #[test]
    fn mm_paste_off_gives_the_key_back_to_the_session() {
        // For vim's visual block, which is what Ctrl-V is for anyone not
        // pasting screenshots.
        let mut f = KeyFilter::default();
        f.set_paste(false);
        assert_eq!(f.filter(&[PASTE_KEY]), forwarded(&[PASTE_KEY]));
    }

    #[test]
    fn the_paste_key_is_only_the_paste_key_in_focus() {
        // In control mode it is an unbound key like any other: back to focus,
        // both bytes through.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, PASTE_KEY]), forwarded(&[KEY, PASTE_KEY]));
    }

    #[test]
    fn ordinary_input_passes_through() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"ls -la\r"), forwarded(b"ls -la\r"));
    }

    #[test]
    fn d_in_control_mode_detaches_without_forwarding_it() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[b'a', b'b', b'c', KEY, b'd']),
            Keystrokes {
                forward: b"abc".to_vec(),
                action: Some(Action::Detach),
                mode: Mode::Focus,
            }
        );
    }

    #[test]
    fn control_mode_stays_on_so_tab_walks_the_list() {
        // The point of the mode: one key, then as many hops as you like.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        assert_eq!(
            f.filter(b"\t"),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        assert_eq!(
            f.filter(b"p"),
            asked(Action::Switch(Motion::Previous), Mode::Control)
        );
        assert_eq!(
            f.filter(b"l"),
            asked(Action::Switch(Motion::Last), Mode::Control)
        );
        assert_eq!(
            f.filter(b"n"),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
    }

    /// The way out of a control mode a switch left on, for a hand that reaches
    /// for the mode key rather than for `Esc`. Nothing reaches the session on
    /// the way: the key was not asking for a literal one.
    #[test]
    fn the_mode_key_leaves_a_control_mode_that_a_switch_left_on() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        assert_eq!(f.filter(&[KEY]), forwarded(b""));
        assert_eq!(f.filter(b"ls"), forwarded(b"ls"));
    }

    #[test]
    fn a_literal_mode_key_still_takes_two_of_them_while_walking_the_sessions() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        // The first goes back to focus, the second starts a fresh mode key, and
        // the third is the one that goes through.
        assert_eq!(f.filter(&[KEY, KEY, KEY]), forwarded(&[KEY]));
        assert_eq!(f.filter(b"x"), forwarded(b"x"));
    }

    #[test]
    fn shift_tab_goes_back_and_a_bare_escape_returns_to_focus() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, 0x1b, b'[', b'Z']),
            asked(Action::Switch(Motion::Previous), Mode::Control)
        );
        // An Esc with nothing behind it in the same read is a real Esc, and
        // typing carries on into the session.
        assert_eq!(f.filter(b"\x1b"), forwarded(b""));
        assert_eq!(f.filter(b"ls"), forwarded(b"ls"));
    }

    #[test]
    fn enter_returns_to_focus_without_reaching_the_session() {
        // Swallowed rather than forwarded: coming back to focus must not also
        // submit whatever is sitting at the prompt.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'\r']), forwarded(b""));
        assert_eq!(f.filter(b"x"), forwarded(b"x"));
    }

    #[test]
    fn a_mistyped_mode_key_returns_to_focus_and_keeps_your_keystrokes() {
        // Both bytes through, so the line is visibly wrong rather than
        // silently eaten while the mode sat there unnoticed.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'g', b'i', b't']),
            forwarded(&[KEY, b'g', b'i', b't'])
        );
    }

    #[test]
    fn the_mode_key_twice_sends_one_through() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, KEY]), forwarded(&[KEY]));
    }

    /// Two presses with a pause in between are a look at the mode and a way
    /// back out of it, not the sequence that means the byte. Sending one there
    /// would put a `^]` into whatever is running for no reason the hand can
    /// remember.
    #[test]
    fn the_mode_key_twice_slowly_only_goes_in_and_out_of_control() {
        let mut f = KeyFilter::default();
        let at = Instant::now();
        assert_eq!(
            f.filter_at(&[KEY], at),
            Keystrokes {
                forward: vec![],
                action: None,
                mode: Mode::Control,
            }
        );
        assert_eq!(f.filter_at(&[KEY], at + LITERAL), forwarded(b""));
        assert_eq!(f.filter_at(b"ls", at + LITERAL), forwarded(b"ls"));
    }

    /// A hand that is not fast, typing the sequence that means the byte across
    /// two reads. The window is for telling the two apart, not a reflex test.
    #[test]
    fn the_second_mode_key_still_counts_a_beat_later() {
        let mut f = KeyFilter::default();
        let at = Instant::now();
        assert_eq!(
            f.filter_at(&[KEY], at),
            Keystrokes {
                forward: vec![],
                action: None,
                mode: Mode::Control,
            }
        );
        assert_eq!(
            f.filter_at(&[KEY], at + Duration::from_millis(700)),
            forwarded(&[KEY])
        );
    }

    #[test]
    fn an_unbound_key_forwards_both() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'x']), forwarded(&[KEY, b'x']));
    }

    #[test]
    fn the_mode_carries_across_reads() {
        // A slow typist splits the sequence across two reads.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY]),
            Keystrokes {
                forward: vec![],
                action: None,
                mode: Mode::Control,
            }
        );
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
    }
}
