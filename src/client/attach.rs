//! Driving a real terminal from an attached session.
//!
//! The terminal-specific half of the client. A mobile app skips this entirely
//! and drives [`crate::client::Attached`] directly, feeding the bytes to its
//! own terminal widget.
//!
//! The client stays deliberately dumb: raw mode, forward keystrokes, paint what
//! arrives, watch for the detach key. All the state lives on the server, which
//! is what makes detaching free.
//!
//! Whose screen it paints on is [`crate::client::screen`]'s to say, and the two
//! answers are different enough to be worth reading before this file.

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

/// A key as a terminal in one of the extended-keys modes spells it.
///
/// The ordinary encoding cannot say which key produced a control byte: Ctrl-],
/// Ctrl-3 and a literal 0x1d all arrive as 0x1d. A program that wants to tell
/// Shift-Enter from Enter asks the terminal for a protocol that can, and from
/// then on every chord arrives as an escape sequence instead of the byte the
/// client was watching for. `pi` asks for it on startup, `helix` and `neovim`
/// on request, so the mode key has to be recognised in all three spellings or
/// a session running one of them cannot be left.
#[derive(Debug, PartialEq, Eq)]
struct Encoded {
    /// How many bytes of the input the sequence took.
    len: usize,
    /// The key, as the codepoint it types unmodified: `]` for Ctrl-].
    code: u32,
    /// Which modifiers were down, in the protocol's bits, with the `+1` it
    /// adds to them taken back off.
    mods: u8,
    /// 1 press, 2 repeat, 3 release. Only a press is a keystroke.
    event: u8,
}

const SHIFT: u8 = 1;
const CTRL: u8 = 4;
/// Caps and num lock, which are reported alongside the modifiers actually held
/// and must not stop a chord from matching.
const LOCKS: u8 = 64 | 128;

const PRESS: u8 = 1;

/// The key codes control mode reads. No letters: one typed without modifiers
/// still arrives as itself, whatever protocol is on.
const ESCAPE: u32 = 27;
const ENTER: u32 = 13;
const TAB: u32 = 9;
/// The modifier keys, which report presses and releases of their own once a
/// program asks for event types. 57441 to 57454 is both shifts, controls,
/// alts, supers, hypers and metas, and the two ISO level shifts.
const MODIFIER_KEYS: std::ops::RangeInclusive<u32> = 57441..=57454;

impl Encoded {
    /// Read one key off the front of `input`, if what is there is one.
    ///
    /// Both spellings: kitty's `CSI code[:alt] ; mods[:event] u`, and the older
    /// `CSI 27 ; mods ; code ~` that xterm's `modifyOtherKeys` sends, which is
    /// what a program falls back to when the terminal does not know the first.
    /// A sequence split across two reads is not one: its bytes go to the
    /// session and the keystroke is missed, the same way a Shift-Tab split down
    /// the middle is.
    fn parse(input: &[u8]) -> Option<Self> {
        let rest = input.strip_prefix(b"\x1b[")?;
        let end = rest.iter().position(|b| (0x40..=0x7e).contains(b))?;
        let mut fields = std::str::from_utf8(&rest[..end]).ok()?.split(';');
        let (code, mods, event) = match rest[end] {
            b'u' => {
                let code = number(fields.next()?)?;
                let (mods, event) = modifiers(fields.next());
                (code, mods, event)
            }
            // The older spelling puts the key last and always names 27 first,
            // which is what tells it from `CSI 2 ~` and the other keys that end
            // the same way.
            b'~' => {
                if number(fields.next()?)? != ESCAPE {
                    return None;
                }
                let (mods, event) = modifiers(fields.next());
                (number(fields.next()?)?, mods, event)
            }
            _ => return None,
        };
        Some(Self {
            len: 2 + end + 1,
            code,
            mods,
            event,
        })
    }

    /// Whether this is the terminal's spelling of a control byte the client
    /// watches for, held with ctrl and nothing else.
    fn is(&self, byte: u8) -> bool {
        self.code == key_code(byte) && self.mods & !LOCKS == CTRL
    }

    /// Whether it is a modifier pressed on its own, which is not a keystroke.
    fn is_modifier(&self) -> bool {
        MODIFIER_KEYS.contains(&self.code)
    }
}

/// The first number in a parameter, ignoring the sub-parameters behind it:
/// those are the alternate keys, which say what the same press would type on
/// another layout and are not what we match on.
fn number(field: &str) -> Option<u32> {
    field.split(':').next()?.parse().ok()
}

/// The modifier field: a bitmask plus one, with the event type behind a colon.
/// An absent field means no modifiers and a press, which is what the protocol
/// says a missing one means.
fn modifiers(field: Option<&str>) -> (u8, u8) {
    let Some(field) = field else {
        return (0, PRESS);
    };
    let mut parts = field.split(':');
    let mods = parts
        .next()
        .and_then(|p| p.parse::<u8>().ok())
        .map_or(0, |m| m.saturating_sub(1));
    let event = parts.next().and_then(|p| p.parse().ok()).unwrap_or(PRESS);
    (mods, event)
}

/// The codepoint a terminal reports for the key behind a control byte. Ctrl-]
/// is 0x1d and the key is `]`; Ctrl-B is 0x02 and the key is `b`, unshifted,
/// which is the form these protocols report. Space is the one the arithmetic
/// does not reach, since a terminal masks it to NUL along with `@`.
fn key_code(byte: u8) -> u32 {
    match byte {
        0 => u32::from(b' '),
        b => u32::from((b | 0x40).to_ascii_lowercase()),
    }
}

/// Which way a switch key moves, through the sessions on a machine or through
/// the machines themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Motion {
    Next,
    Previous,
    /// Back to the one you came from.
    Last,
    /// The first session on the next machine.
    NextHost,
    PreviousHost,
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
/// sessions on the machine you are on, and `h` is how you change machine.
/// `Esc`, `Enter` or the mode key goes back to focus, `d` detaches,
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
    /// How the terminal spelled the key most recently taken from it: the bare
    /// control byte, or the escape sequence an extended-keys terminal sends in
    /// its place. What goes to the session when the key turns out to be the
    /// session's after all has to be what the terminal sent, or a program that
    /// asked for the long spelling gets a byte it no longer reads that way.
    spelling: Vec<u8>,
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
            spelling: vec![prefix],
        }
    }

    /// How the terminal spelled the last key the client took for itself, for a
    /// caller that decides to hand it back: the paste key, when there turns out
    /// to be no image on the clipboard.
    pub fn spelling(&self) -> &[u8] {
        &self.spelling
    }

    /// Remember a spelling, for the paths that give the key to the session.
    fn spell(&mut self, bytes: &[u8]) {
        self.spelling.clear();
        self.spelling.extend_from_slice(bytes);
    }

    /// Where an action leaves the keyboard. A switch stays in control mode, so
    /// the next key carries on walking without a mode key of its own.
    fn after(action: Action) -> Mode {
        match action {
            Action::Detach | Action::Paste => Mode::Focus,
            Action::Switch(_) => Mode::Control,
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
            // A terminal that a program has put into an extended-keys mode
            // spells the client's own keys as escape sequences, so they have to
            // be read off before the bytes are looked at or the mode key goes
            // past unnoticed.
            if let Some(key) = Encoded::parse(&input[i..]) {
                let spelling = &input[i..i + key.len];
                i += key.len;
                if let Some(action) = self.encoded(&key, spelling, now, &mut forward) {
                    self.mode = Self::after(action);
                    return Keystrokes {
                        forward,
                        action: Some(action),
                        mode: self.mode,
                    };
                }
                continue;
            }
            let b = input[i];
            i += 1;
            if self.mode == Mode::Focus {
                if b == self.prefix {
                    self.spell(&[b]);
                    self.mode = Mode::Control;
                    self.pressed = Some(now);
                } else if self.paste && b == PASTE_KEY {
                    self.spell(&[b]);
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
                    self.spell(&[b]);
                    if pressed.is_some_and(|at| now.duration_since(at) < LITERAL) {
                        forward.push(b);
                    }
                    self.mode = Mode::Focus;
                    None
                }
                b'd' | b'D' => Some(Action::Detach),
                b'\t' | b'n' | b'N' => Some(Action::Switch(Motion::Next)),
                b'p' | b'P' => Some(Action::Switch(Motion::Previous)),
                b'l' | b'L' => Some(Action::Switch(Motion::Last)),
                // The one key that reads its own case, because shift already
                // means backwards here: `H` is to `h` what shift-tab is to tab,
                // rather than a second letter to remember.
                b'h' => Some(Action::Switch(Motion::NextHost)),
                b'H' => Some(Action::Switch(Motion::PreviousHost)),
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
                    forward.extend_from_slice(&self.spelling);
                    forward.push(other);
                    None
                }
            };
            // Whatever is left of the chunk is dropped: nobody types through a
            // detach or a switch.
            if let Some(action) = action {
                self.mode = Self::after(action);
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

    /// One key in the terminal's own spelling. Returns what it asked for, if it
    /// asked for anything; everything else it does, it does to `forward` and to
    /// the mode.
    fn encoded(
        &mut self,
        key: &Encoded,
        spelling: &[u8],
        now: Instant,
        forward: &mut Vec<u8>,
    ) -> Option<Action> {
        if self.mode == Mode::Focus {
            if key.event != PRESS {
                // The session's own keys keep their releases, since it asked to
                // be told about them. The client's do not: the press they
                // belong to never got there either.
                if !key.is(self.prefix) && !(self.paste && key.is(PASTE_KEY)) {
                    forward.extend_from_slice(spelling);
                }
                return None;
            }
            if key.is(self.prefix) {
                self.spell(spelling);
                self.mode = Mode::Control;
                self.pressed = Some(now);
                return None;
            }
            if self.paste && key.is(PASTE_KEY) {
                self.spell(spelling);
                return Some(Action::Paste);
            }
            forward.extend_from_slice(spelling);
            return None;
        }

        // Dropping everything that is not a press is what makes control mode
        // usable at all once a program has asked for event types: the ctrl you
        // were holding reports its own release the moment you let go of the
        // mode key, and reading that as a key would drop you back to focus
        // before you had typed anything.
        if key.event != PRESS || key.is_modifier() {
            return None;
        }
        let pressed = self.pressed.take();
        if key.is(self.prefix) {
            self.spell(spelling);
            if pressed.is_some_and(|at| now.duration_since(at) < LITERAL) {
                forward.extend_from_slice(spelling);
            }
            self.mode = Mode::Focus;
            return None;
        }
        match key.code {
            TAB if key.mods & SHIFT != 0 => return Some(Action::Switch(Motion::Previous)),
            TAB => return Some(Action::Switch(Motion::Next)),
            ESCAPE | ENTER => {
                self.mode = Mode::Focus;
                return None;
            }
            _ => {}
        }
        // An unbound key, handled the way the plain spelling of one is: back to
        // focus, with what opened the mode and what closed it both passed on.
        self.mode = Mode::Focus;
        forward.extend_from_slice(&self.spelling);
        forward.extend_from_slice(spelling);
        None
    }
}

#[cfg(feature = "desktop")]
pub use terminal::{Held, hold, run, session_size, terminal_size};

#[cfg(feature = "desktop")]
mod terminal {
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

    use super::{Action, KeyFilter, Mode, Outcome};
    use crate::client::screen::ScreenMode;
    use crate::client::status::{self, Filter, Status};
    use crate::client::{Attached, SessionHalves, SessionReader, SessionWriter, Update};
    use crate::clipboard;
    use crate::notify;
    use crate::proto::{HostedEvent, Size};
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

    /// Run the attach loop until the client detaches, switches away, or the
    /// session ends.
    ///
    /// `mode` is where the keyboard starts, which is how a hop carries control
    /// mode through the reattach.
    pub async fn run(
        held: &mut Held,
        session: Attached,
        target: &str,
        mode: Mode,
    ) -> Result<Outcome> {
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
        pump(
            &mut held.keys,
            session,
            status,
            mode,
            target,
            screen,
            on_alternate,
        )
        .await
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

    async fn pump(
        keyboard: &mut mpsc::Receiver<Vec<u8>>,
        session: Attached,
        mut status: Status,
        mode: Mode,
        target: &str,
        screen: Screen,
        on_alternate: Arc<AtomicBool>,
    ) -> Result<Outcome> {
        let takes_pastes = session.paste;
        let SessionHalves {
            mut reader,
            mut writer,
        } = session.split();
        let mut stdout = tokio::io::stdout();
        let mut winch = signal(SignalKind::window_change())?;
        let mut keys = KeyFilter::default();
        keys.set_mode(mode);
        let mut output = Filter::new(screen);
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
        let bells = Bells::new(target);

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
                        stdout.write_all(&output.feed(&bytes)).await?;
                        on_alternate.store(output.on_alternate(), Ordering::Relaxed);
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
        fn new(target: &str) -> Self {
            Self {
                host: target.rsplit_once('/').map(|(host, _)| host.to_string()),
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
                    // before there has been a key to press.
                    Update::History(_) => {}
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
            // History counts as output here: a caller collecting a session's
            // bytes asked for the lines it asked for.
            Update::Output(bytes) | Update::Screen(bytes) | Update::History(bytes) => {
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
            // For a caller with no terminal to raise one on. A client that
            // renders sessions itself reads these from its own subscription.
            Update::Event(_) => continue,
            Update::Exited(code) => Outcome::Exited(code),
            Update::Disconnected => Outcome::Disconnected,
        };
        return Ok(Collected { output, outcome });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "desktop")]
    use crate::settings::Screen;

    /// Keystrokes that went straight through, leaving the keyboard in focus.
    fn forwarded(bytes: &[u8]) -> Keystrokes {
        Keystrokes {
            forward: bytes.to_vec(),
            action: None,
            mode: Mode::Focus,
        }
    }

    /// A key that took the keyboard into control mode without asking for
    /// anything or reaching the session.
    fn held() -> Keystrokes {
        Keystrokes {
            forward: vec![],
            action: None,
            mode: Mode::Control,
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
        let reset = terminal::reset(Screen::Alternate.mode(), false);
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
        let reset = terminal::reset(Screen::Alternate.mode(), false);
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
        let takeover = terminal::takeover(Screen::Alternate.mode(), false);
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
        let takeover = terminal::takeover(Screen::Alternate.mode(), false);
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
        let takeover = terminal::takeover(Screen::Alternate.mode(), false);
        for sequence in ["\x1b[?1049l", "\x1b[?1047l", "\x1b[23;2t"] {
            assert!(
                !takeover.contains(sequence),
                "hopping sends {sequence:?}, which gives the terminal back mid-attach"
            );
        }
    }

    /// The bug this was written for: `pi` asks the terminal for the kitty
    /// keyboard protocol on startup, and from then on the mode key arrives as
    /// `CSI 93 ; 5 u` (`]` held with ctrl) rather than as 0x1d, so a session
    /// running it could not be left.
    #[test]
    fn the_mode_key_is_still_the_mode_key_spelt_as_an_escape_sequence() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(b"ls\x1b[93;5ud"),
            Keystrokes {
                forward: b"ls".to_vec(),
                action: Some(Action::Detach),
                mode: Mode::Focus,
            }
        );
    }

    /// The other spelling, which is what a program falls back to on a terminal
    /// that does not know the first.
    #[test]
    fn modify_other_keys_spells_the_mode_key_too() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(b"\x1b[27;5;93~d"),
            asked(Action::Detach, Mode::Focus)
        );
    }

    /// The alternate keys a terminal reports alongside the key, which say what
    /// the same press would type on another layout, and the lock keys, which
    /// are reported whether or not they are part of the chord.
    #[test]
    fn the_mode_key_matches_past_what_the_terminal_adds_to_it() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93:125;5u"), held());
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
        // Caps lock is bit 64, on top of ctrl's 4: 1 + 4 + 64.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;69u"), held());
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
    }

    /// A chord that is not the mode key goes to the session as the terminal
    /// spelt it, byte for byte.
    #[test]
    fn another_chord_in_the_same_spelling_passes_through() {
        let mut f = KeyFilter::default();
        // Ctrl-A, and the same key with shift as well, which is not the key
        // either even though `]` is in it.
        assert_eq!(f.filter(b"\x1b[97;5u"), forwarded(b"\x1b[97;5u"));
        assert_eq!(f.filter(b"\x1b[93;6u"), forwarded(b"\x1b[93;6u"));
    }

    /// Once a program asks for event types, letting go of the key reports
    /// itself, and so does the ctrl that was held with it. Read as keystrokes
    /// they would take the mode away again before anything could be typed
    /// into it.
    #[test]
    fn releases_are_not_keystrokes_and_do_not_leave_control_mode() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u"), held());
        // The mode key released, then the ctrl key itself released.
        assert_eq!(f.filter(b"\x1b[93;5:3u"), held());
        assert_eq!(f.filter(b"\x1b[57442;5:3u"), held());
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
    }

    /// A release in focus mode is the session's, and goes to it, unless the
    /// press it belongs to was the client's and never got there.
    #[test]
    fn the_session_keeps_the_releases_of_its_own_keys() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[97;5:3u"), forwarded(b"\x1b[97;5:3u"));
        assert_eq!(f.filter(b"\x1b[93;5:3u"), forwarded(b""));
    }

    #[test]
    fn the_escape_sequence_spelling_of_the_mode_key_twice_sends_one_through() {
        // What goes to the session is what the terminal sent, since a program
        // that asked for the long spelling is no longer reading the byte.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u\x1b[93;5u"), forwarded(b"\x1b[93;5u"));
    }

    #[test]
    fn control_mode_reads_escape_and_shift_tab_in_the_long_spelling() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u\x1b[9;2u"), {
            asked(Action::Switch(Motion::Previous), Mode::Control)
        });
        assert_eq!(f.filter(b"\x1b[27u"), forwarded(b""));
        assert_eq!(f.filter(b"ls"), forwarded(b"ls"));
    }

    #[test]
    fn an_unbound_key_in_the_long_spelling_forwards_both() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[122;5u"),
            forwarded(b"\x1b[93;5u\x1b[122;5u")
        );
    }

    #[test]
    fn the_paste_key_is_read_in_the_long_spelling_as_well() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(b"ls\x1b[118;5u"),
            Keystrokes {
                forward: b"ls".to_vec(),
                action: Some(Action::Paste),
                mode: Mode::Focus,
            }
        );
        // And what goes back to the session, when the clipboard turns out to
        // have nothing on it to paste, is what the terminal sent.
        assert_eq!(f.spelling(), b"\x1b[118;5u");
    }

    #[test]
    fn a_sequence_that_is_not_a_key_is_left_alone() {
        let mut f = KeyFilter::default();
        // Arrow keys, a bracketed paste, and a mouse report: all of them end in
        // bytes a key sequence never does, or start with a number that is not
        // the one the long spelling of a key starts with.
        for sequence in [
            &b"\x1b[A"[..],
            b"\x1b[1;5C",
            b"\x1b[200~hello\x1b[201~",
            b"\x1b[<0;12;24M",
            b"\x1b[2~",
        ] {
            assert_eq!(f.filter(sequence), forwarded(sequence));
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

    #[test]
    fn the_host_keys_move_machine_and_leave_control_mode_on() {
        // The one binding that reads its own case, so both spellings are keys
        // rather than `H` dropping back to focus as an unbound one would.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'h']),
            asked(Action::Switch(Motion::NextHost), Mode::Control)
        );
        assert_eq!(
            f.filter(b"H"),
            asked(Action::Switch(Motion::PreviousHost), Mode::Control)
        );
        // And walking the machine you land on carries on from there.
        assert_eq!(
            f.filter(b"\t"),
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
