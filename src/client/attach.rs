//! Driving a real terminal from an attached session.
//!
//! The terminal-specific half of the client. A mobile app skips this entirely
//! and drives [`crate::client::Attached`] directly, feeding the bytes to its
//! own terminal widget.
//!
//! The client stays deliberately dumb: raw mode, forward keystrokes, paint what
//! arrives, watch for the detach key. All the state lives on the server, which
//! is what makes detaching free.

use anyhow::Result;

use crate::client::{Attached, SessionHalves, Update};

/// The key that goes from focus mode to control mode: Ctrl-` (0x00).
///
/// Not tmux's Ctrl-b or screen's Ctrl-a, because you are quite likely running
/// one of those *inside* a manymux session: manymux has no panes or tabs, so
/// splitting a window is still their job, and taking their prefix would mean
/// swallowing it before it ever reached them.
///
/// What arrives is a NUL, and Ctrl-Space and Ctrl-@ send the same one: a
/// terminal masks the top bits off the character, and backtick, space and `@`
/// come out identical. So all three keys reach this, which is the point on
/// macOS, where Ctrl-Space is taken by input-source switching and never gets
/// to the terminal at all. Emacs wants the byte for set-mark, which is what
/// pressing the key twice is for: it sends one through.
pub const DEFAULT_PREFIX: u8 = 0x00;

/// The key in force, from `MM_PREFIX` if it is set and usable.
///
/// Accepts ``C-` ``, `C-b`, `^B`, `C-Space` or `\x02`. An unusable value is a
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
            eprintln!("mm: MM_PREFIX={text:?} is not a control key; using Ctrl-`");
            DEFAULT_PREFIX
        }
    }
}

/// Parse a control key: ``C-` ``, `C-b`, `c-B`, `^b`, a bare `b`, `C-Space`, or
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

    // Spelled out, because `MM_PREFIX=C- ` is not something anyone would type,
    // and because it is the same byte as the default anyway.
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
    // arithmetic covers `C-\`, `C-]` and friends just past `Z`, and the
    // backtick just past them, which is the default and comes out a NUL.
    let byte = u8::try_from(key).ok()?.to_ascii_uppercase();
    (0x40..=0x60).contains(&byte).then_some(byte & 0x1f)
}

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
/// sessions. `Esc` or `Enter` goes back to focus, `d` detaches, and the key
/// pressed twice sends one through for whatever wants it inside the session.
/// Any other key drops back to focus and passes both bytes through unchanged,
/// so a mistyped mode key costs you visible junk rather than a silently
/// swallowed line.
pub struct KeyFilter {
    prefix: u8,
    mode: Mode,
    /// Whether the key that turned control mode on is the last one pressed.
    ///
    /// It matters for exactly one key: its own. Pressed straight after itself
    /// it sends one through, and pressed in a control mode that a switch left
    /// on it starts over instead. So the key always starts a mode key, two in a
    /// row always send one, and `<key> d` detaches whether or not you were
    /// already walking the sessions.
    fresh: bool,
}

impl Default for KeyFilter {
    fn default() -> Self {
        Self::new(prefix())
    }
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
            mode: Mode::Focus,
            fresh: false,
        }
    }

    /// Start in a mode. Attaching after a switch starts in control mode, which
    /// is what makes `tab tab tab` walk the list across the hops.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.fresh = false;
    }

    pub fn filter(&mut self, input: &[u8]) -> Keystrokes {
        let mut forward = Vec::with_capacity(input.len());
        let mut i = 0;
        while i < input.len() {
            let b = input[i];
            i += 1;
            if self.mode == Mode::Focus {
                if b == self.prefix {
                    self.mode = Mode::Control;
                    self.fresh = true;
                } else {
                    forward.push(b);
                }
                continue;
            }
            let was_fresh = std::mem::take(&mut self.fresh);
            let action = match b {
                // First, so that a mode key which is itself one of the keys
                // below can still be sent through by pressing it twice.
                b if b == self.prefix => {
                    if was_fresh {
                        forward.push(self.prefix);
                        self.mode = Mode::Focus;
                    } else {
                        self.fresh = true;
                    }
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
                    Action::Detach => Mode::Focus,
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

    use anyhow::{Result, bail};
    use crossterm::terminal;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::signal::unix::{SignalKind, signal};

    use super::{Action, KeyFilter, Mode, Outcome};
    use crate::client::status::{self, Filter, Status};
    use crate::client::{Attached, SessionHalves, Update};
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

    async fn pump(session: Attached, mut status: Status, mode: Mode) -> Result<Outcome> {
        let SessionHalves {
            mut reader,
            mut writer,
        } = session.split();
        let mut stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut winch = signal(SignalKind::window_change())?;
        let mut keys = KeyFilter::default();
        keys.set_mode(mode);
        let mut output = Filter::default();
        let mut buf = vec![0u8; 8192];
        // The row no longer says what the keys are doing. Held until there is a
        // safe moment to draw, the same as a mark the session cleared.
        let mut restate = false;

        loop {
            tokio::select! {
                n = stdin.read(&mut buf) => {
                    let n = match n {
                        Ok(0) | Err(_) => return Ok(Outcome::Detached),
                        Ok(n) => n,
                    };
                    let keystrokes = keys.filter(&buf[..n]);
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
                        None => {}
                    }
                }
                update = reader.next() => match update? {
                    Update::Output(bytes) => {
                        stdout.write_all(&output.feed(&bytes)).await?;
                        // Only between sequences: a repaint written into the
                        // middle of one would corrupt it. Whatever cleared the
                        // mark stays noted until there is a safe moment.
                        if output.at_boundary() && (output.take_dirty() || restate) {
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
            Update::Output(bytes) => {
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

    /// The mode key, whatever it is. Written out because it cannot be typed
    /// into a byte string: the default is Ctrl-`, which is a NUL.
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
        assert_eq!(parse_prefix("C-]"), Some(0x1d));
    }

    #[test]
    fn every_spelling_of_the_default_key_is_the_same_byte() {
        // A terminal clears the top bits off the character, so the backtick,
        // space and `@` all arrive as a NUL and are one key as far as this is
        // concerned. `MM_PREFIX=C- ` is not something anyone would write, so
        // the word stands in for that one.
        assert_eq!(parse_prefix("C-`"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("^`"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("C-@"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("C-Space"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("c-space"), Some(DEFAULT_PREFIX));
        assert_eq!(parse_prefix("^Space"), Some(DEFAULT_PREFIX));
        // And the byte itself, however it got into the variable.
        assert_eq!(parse_prefix("\0"), Some(DEFAULT_PREFIX));
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

    /// The habit that would otherwise put a stray byte into whatever is
    /// running: the mode key and `d` typed together without noticing that a
    /// switch had left control mode on.
    #[test]
    fn the_detach_habit_still_detaches_while_walking_the_sessions() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        assert_eq!(f.filter(&[KEY, b'd']), asked(Action::Detach, Mode::Focus));
    }

    #[test]
    fn a_literal_mode_key_still_takes_two_of_them_from_inside_control_mode() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Switch(Motion::Next), Mode::Control)
        );
        // The first starts over rather than being sent, the second is the one
        // that goes through, and the keyboard is back in focus afterwards.
        assert_eq!(f.filter(&[KEY, KEY]), forwarded(&[KEY]));
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
