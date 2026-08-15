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

use crate::client::scroll;
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
    /// What the same key types with shift held, when the terminal was asked to
    /// report alternates. `H` for the `h` key.
    shifted: Option<u32>,
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

/// The key codes control mode and the prompts read. No letters: one typed
/// without modifiers still arrives as itself, whatever protocol is on.
const ESCAPE: u32 = 27;
const ENTER: u32 = 13;
const TAB: u32 = 9;
const BACKSPACE: u32 = 127;
/// The modifier keys, which report presses and releases of their own once a
/// program asks for event types. 57441 to 57454 is both shifts, controls,
/// alts, supers, hypers and metas, and the two ISO level shifts.
const MODIFIER_KEYS: std::ops::RangeInclusive<u32> = 57441..=57454;
/// The Unicode private use area, which is where the protocol puts every key
/// that types nothing at all: the arrows, the function keys, the keypad, the
/// modifiers above.
const FUNCTIONAL: std::ops::RangeInclusive<u32> = 57344..=63743;

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
        let mut shifted = None;
        let (code, mods, event) = match rest[end] {
            b'u' => {
                let field = fields.next()?;
                let code = number(field)?;
                shifted = alternate(field);
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
            shifted,
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

    /// The byte this key stands for at a prompt, if it is one of the few that
    /// mean anything there.
    ///
    /// The editing is written once, against the ordinary bytes, and a terminal
    /// in an extended-keys mode spells the same keys the long way: reading them
    /// back to the byte is what keeps the two spellings one prompt. Everything
    /// else a program's mode brings with it (releases, modifiers, the arrows,
    /// the mode key itself) is not text and stands for nothing.
    fn typed(&self) -> Option<u8> {
        if self.event != PRESS {
            return None;
        }
        match self.code {
            // Ctrl-U, the one chord the prompt reads.
            _ if self.is(0x15) => Some(0x15),
            _ => self.byte(),
        }
    }

    /// The byte the ordinary encoding would have sent for this key, where there
    /// is one.
    ///
    /// What the client's modes read is bytes, and this is what lets each of
    /// them keep one table rather than one per protocol. A key that types text
    /// only arrives spelt this way at all once a program asks for *report all
    /// keys as escape codes*; with the flags `pi` sets it still arrives as the
    /// character it types. Reading both costs a line and means the client's own
    /// keys keep working whichever of the two a program asks the terminal for.
    fn byte(&self) -> Option<u8> {
        match self.code {
            ENTER => Some(b'\r'),
            ESCAPE => Some(0x1b),
            BACKSPACE => Some(0x7f),
            TAB => Some(b'\t'),
            // ASCII and no wider: `u8::try_from` would take a character up to
            // U+00FF and hand back the one byte of it, which is half a
            // character everywhere it is written.
            _ => self.text().filter(char::is_ascii).map(|typed| typed as u8),
        }
    }

    /// The character this key types, if it types one.
    ///
    /// A chord types nothing: ctrl, alt and super are how the client's own keys
    /// and the session's are spelt, and they are read by the control byte
    /// behind them rather than by a letter. Shift does type, and what it types
    /// is the alternate the terminal reports beside the key when it was asked
    /// for alternates. Without that, an ASCII letter is upper-cased here rather
    /// than guessed at further: the client's keys read their own case (`h` and
    /// `H`), and a layout it cannot see is not worth more than that.
    fn text(&self) -> Option<char> {
        // Letting go of a key types nothing, and neither does holding it down
        // long enough to repeat: a mode that reports both would otherwise type
        // a character two and three times over.
        if self.event != PRESS || self.mods & !(SHIFT | LOCKS) != 0 {
            return None;
        }
        let shift = self.mods & SHIFT != 0;
        let code = self.shifted.filter(|_| shift).unwrap_or(self.code);
        // The codepoints the protocol keeps for keys that type nothing: the
        // arrows, the function keys, the keypad, the modifiers themselves.
        if FUNCTIONAL.contains(&code) {
            return None;
        }
        let typed = char::from_u32(code).filter(|c| !c.is_control())?;
        Some(match self.shifted {
            None if shift => typed.to_ascii_uppercase(),
            _ => typed,
        })
    }
}

/// The alternate key in a parameter: what the same press would type with shift
/// held, which is the first sub-parameter when the terminal was asked to report
/// alternates. The one behind it is the base layout's key, which is for
/// matching shortcuts on a layout that is not the one in use, and is not what
/// anything here is doing.
fn alternate(field: &str) -> Option<u32> {
    field.split(':').nth(1)?.parse().ok()
}

/// How many bytes the escape sequence at the head of `input` takes, if what is
/// there is a sequence at all rather than the Esc key itself.
///
/// A sequence that the chunk ends in the middle of is not one, the same way a
/// Shift-Tab split down the middle is not one: what is there is an Esc, which
/// is what it would have been read as before any of this.
fn sequence(input: &[u8]) -> Option<usize> {
    let rest = input.strip_prefix(b"\x1b")?;
    match rest.first()? {
        // A control sequence, which runs to its final byte.
        b'[' => {
            let end = rest[1..].iter().position(|b| (0x40..=0x7e).contains(b))?;
            Some(3 + end)
        }
        // SS3, which is one byte and is how a terminal in application-keypad
        // mode spells the arrows.
        b'O' => rest.get(1).map(|_| 3),
        _ => None,
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
    /// Move the view over the session's history, or open or close it. Where it
    /// is and what it shows is [`super::scroll`]'s; this only says which way.
    Scroll(Scroll),
    /// Something happened to the search. The text being typed lives in the key
    /// filter, since that is what a keyboard mode is; everything done with it
    /// is the caller's.
    Find(Find),
    /// The same, for the prompt that gives the session a title.
    Rename(Rename),
}

/// What a key did to the search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Find {
    /// The prompt is open and empty. Draw it.
    Open,
    /// The needle changed. Draw it again.
    Typed,
    /// Enter: go and look for what has been typed.
    Run,
    /// Esc with a prompt open: the view stays, the prompt goes.
    Cancel,
    /// The next match further back, and the one back towards the live screen.
    Next,
    Previous,
}

/// What a key did to the rename prompt.
///
/// The same four as a search, minus the two that walk matches: a name is typed
/// and then either sent or given up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rename {
    /// The prompt is open and empty. Draw it.
    Open,
    /// The name changed. Draw it again.
    Typed,
    /// Enter: ask the host for the name that has been typed. It answers with
    /// the name that stuck, or with why it refused.
    Run,
    /// Esc, or a rub with nothing left to rub out: the name stays as it was.
    Cancel,
}

/// A move through the history. Opening the view is not one of these: any of
/// them arriving while the live screen is showing opens it and then moves it,
/// so a wheel notch does the obvious thing without a mode to learn first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scroll {
    /// Back through the history, in lines.
    Up(u64),
    Down(u64),
    PageUp,
    PageDown,
    /// The oldest line the host still has, and the live screen.
    Top,
    Bottom,
    /// Back to the live screen, and out of the view.
    Leave,
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
    /// The screen is showing the session's history rather than the session, and
    /// the keys move it. Only on a screen the client owns, where the terminal
    /// has no scrollback of its own to offer.
    Scroll,
    /// A name is being typed on the mark row. A mode of its own because the
    /// keyboard here is neither the session's nor doing control keys: every
    /// letter is text, and the row is the only place it shows.
    Rename,
}

/// One mouse report, in the SGR spelling (`CSI < button ; col ; row M`).
///
/// Only that spelling, because it is the one the client asks for when it turns
/// tracking on for itself (`?1006h` beside `?1000h`). The older spellings are a
/// session's own doing, and a session's reports are never read here: while it
/// has the mouse, the client's tracking is off and every report belongs to it.
#[derive(Debug, PartialEq, Eq)]
struct Report {
    len: usize,
    button: u8,
    /// A press. Releases end in `m` and are dropped: the wheel has no release,
    /// and a click's two halves would move the view twice.
    press: bool,
}

/// Buttons 64 and 65, once the modifier bits (shift 4, meta 8, ctrl 16) are
/// taken off. 66 and 67 are the horizontal wheel, which has nowhere to go here.
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;
const MODIFIERS: u8 = 4 | 8 | 16;

impl Report {
    fn parse(input: &[u8]) -> Option<Self> {
        let rest = input.strip_prefix(b"\x1b[<")?;
        let end = rest.iter().position(|b| matches!(b, b'M' | b'm'))?;
        let button = std::str::from_utf8(&rest[..end])
            .ok()?
            .split(';')
            .next()?
            .parse::<u16>()
            .ok()?;
        Some(Self {
            len: 3 + end + 1,
            button: u8::try_from(button).ok()? & !MODIFIERS,
            press: rest[end] == b'M',
        })
    }

    /// Which way this turns the view, if it turns it at all.
    fn scroll(&self) -> Option<Scroll> {
        match (self.press, self.button) {
            (true, WHEEL_UP) => Some(Scroll::Up(scroll::WHEEL)),
            (true, WHEEL_DOWN) => Some(Scroll::Down(scroll::WHEEL)),
            _ => None,
        }
    }
}

/// Whether the wheel is the client's to read, or the terminal's to do what it
/// likes with.
///
/// Only while the view is up. Reporting the mouse is what stops a drag from
/// selecting, so a client that holds it for the whole attach is one you cannot
/// copy a line out of without a modifier held down; and with the view closed
/// there is nothing here for a notch to move anyway. What the view is open on
/// has already been decided by then: it opens only on a screen the client owns
/// and a host new enough to answer for a window.
///
/// Never while the session has asked for reports of its own, which is both
/// halves of the same rule: two readers on one wheel, and a client that turned
/// tracking on would turn it off again on the way out of the view, leaving a
/// program that asked for the mouse without one.
///
/// Desktop-only, like the terminal it is a rule about: a mobile app has no
/// wheel to route and its own idea of what a drag is.
#[cfg(feature = "desktop")]
fn wheel_is_ours(view_open: bool, session_mouse: bool) -> bool {
    view_open && !session_mouse
}

/// The keys that move the view, in the spellings terminals send them in.
///
/// Matched before the bytes are looked at one at a time, so that the escape
/// starting each of them is not mistaken for the bare Esc that leaves.
const VIEW_KEYS: &[(&[u8], Scroll)] = &[
    (b"\x1b[5~", Scroll::PageUp),
    (b"\x1b[6~", Scroll::PageDown),
    (b"\x1b[A", Scroll::Up(1)),
    (b"\x1b[B", Scroll::Down(1)),
    (b"\x1bOA", Scroll::Up(1)), // the application-keypad spellings, which a
    (b"\x1bOB", Scroll::Down(1)), // program may have left the terminal in
    (b"\x1b[H", Scroll::Top),
    (b"\x1b[1~", Scroll::Top),
    (b"\x1b[7~", Scroll::Top),
    (b"\x1b[F", Scroll::Bottom),
    (b"\x1b[4~", Scroll::Bottom),
    (b"\x1b[8~", Scroll::Bottom),
];

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
    /// Whether there is a history to look at: a screen the client owns, on a
    /// host new enough to send windows of one. Off, [`SCROLL_KEY`] is a key
    /// like any other and the session gets it.
    scroll: bool,
    /// Whether the wheel is the client's. True only while the client has mouse
    /// tracking on for itself, which it does only while the session has asked
    /// for none: a report arriving then is one the client caused, so reading it
    /// takes nothing from the session, and dropping the ones that are not the
    /// wheel keeps a stray click from typing into a shell.
    wheel: bool,
    /// What is being typed at a prompt, while one is open, and which prompt
    /// it is.
    ///
    /// Kept here rather than by the caller because it is keyboard state, and
    /// because a chunk holding several typed bytes has to become one line
    /// rather than one action per byte, which is a shape this returns nothing
    /// for.
    prompt: Option<Prompt>,
}

/// A line being typed at one of the client's prompts.
///
/// The editing is the same either way, which is the reason there is one of
/// these rather than one per prompt: a rub is a rub, and only the action
/// handed back says which prompt it happened at.
struct Prompt {
    what: Prompting,
    typed: Vec<u8>,
}

/// Which prompt is open.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Prompting {
    /// A search through the session's history.
    Find,
    /// The session's title.
    Rename,
}

impl Prompting {
    /// The same four things happen at either prompt; which action carries them
    /// is all that differs, so the pair is named at the one place it matters.
    fn action(self, find: Find, rename: Rename) -> Action {
        match self {
            Prompting::Find => Action::Find(find),
            Prompting::Rename => Action::Rename(rename),
        }
    }
}

/// Take the last character off a line being typed, and say whether there was
/// one to take.
///
/// A character rather than a byte, because one press of a key that put three
/// bytes in has to take all three out again. UTF-8 continuation bytes are
/// `0b10xxxxxx`, so the character starts at the last byte that is not one.
fn pop_char(typed: &mut Vec<u8>) -> Option<()> {
    let start = typed.iter().rposition(|b| b & 0xc0 != 0x80)?;
    typed.truncate(start);
    Some(())
}

/// Opens the view, from control mode. tmux's copy-mode key, for the hands that
/// already know it.
const SCROLL_KEY: u8 = b'[';

/// Opens a search, from control mode or from inside the view. What every pager
/// and editor uses, for the same reason.
const FIND_KEY: u8 = b'/';

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
            scroll: false,
            wheel: false,
            prompt: None,
        }
    }

    /// What is being typed at the search prompt, if that is the one open.
    pub fn needle(&self) -> Option<String> {
        self.typed(Prompting::Find)
    }

    /// What is being typed at the rename prompt, if that is the one open.
    pub fn wanted_name(&self) -> Option<String> {
        self.typed(Prompting::Rename)
    }

    fn typed(&self, what: Prompting) -> Option<String> {
        self.prompt
            .as_ref()
            .filter(|prompt| prompt.what == what)
            .map(|prompt| String::from_utf8_lossy(&prompt.typed).into_owned())
    }

    /// Close the prompt, keeping whatever was typed out of the next one.
    pub fn stop_typing(&mut self) {
        self.prompt = None;
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
            Action::Scroll(Scroll::Leave) => Mode::Focus,
            // Every other move keeps the view up, wherever it was opened from:
            // a wheel notch in focus mode opens it and stays.
            Action::Scroll(_) => Mode::Scroll,
            // A search is something you do to the view, so it puts you in it
            // and leaves you there. Cancelling the prompt is cancelling the
            // prompt, not leaving.
            Action::Find(_) => Mode::Scroll,
            // The rename prompt is its own mode, and the only ways out of it
            // are sending the name and giving up on it.
            Action::Rename(Rename::Run | Rename::Cancel) => Mode::Focus,
            Action::Rename(_) => Mode::Rename,
        }
    }

    /// What a key means in control mode, by the byte it arrives as.
    ///
    /// A table of its own rather than a match at the one place keys are read,
    /// because the same keys reach the client in more than one spelling: a
    /// program holding the terminal in an extended-keys mode sends them as
    /// escape sequences, and keys that worked only in the short spelling would
    /// be keys that stop working while such a program is running.
    ///
    /// The prefix and the keys that go back to focus are not here. What they do
    /// to the mode, and what the session sees of them, is the caller's, and it
    /// is the one thing that does differ between the spellings.
    fn controlling(&self, b: u8) -> Option<Action> {
        let action = match b {
            b'd' | b'D' => Action::Detach,
            b'\t' | b'n' | b'N' => Action::Switch(Motion::Next),
            b'p' | b'P' => Action::Switch(Motion::Previous),
            b'l' | b'L' => Action::Switch(Motion::Last),
            // The one key that reads its own case, because shift already means
            // backwards here: `H` is to `h` what shift-tab is to tab, rather
            // than a second letter to remember.
            b'h' => Action::Switch(Motion::NextHost),
            b'H' => Action::Switch(Motion::PreviousHost),
            // Only where there is a history to look at. Elsewhere they are
            // unbound keys, and the session gets them.
            SCROLL_KEY if self.scroll => Action::Scroll(Scroll::Up(0)),
            // Straight from control mode into a search, which is the whole
            // gesture: `Ctrl-] /`, type, Enter.
            FIND_KEY if self.scroll => Action::Find(Find::Open),
            // And the same gesture for the name: `Ctrl-] r`, type, Enter.
            // Nothing here asks the host first, so it is bound whether or not
            // the host turns out to take it: an old one is answered with a
            // sentence on the row, which is more use than a letter that lands
            // in the shell.
            b'r' | b'R' => Action::Rename(Rename::Open),
            _ => return None,
        };
        Some(action)
    }

    /// The same, for the keys that move the view. The search keys come first,
    /// since they are not moves and one of them opens a prompt that takes every
    /// key after it.
    fn scrolling(&self, b: u8) -> Option<Action> {
        let found = match b {
            FIND_KEY => Some(Find::Open),
            b'n' => Some(Find::Next),
            b'N' => Some(Find::Previous),
            _ => None,
        };
        if let Some(found) = found {
            return Some(Action::Find(found));
        }
        let scroll = match b {
            b'q' | 0x1b | b'\r' | b'\n' => Scroll::Leave,
            b if b == self.prefix => Scroll::Leave,
            b' ' | b'f' => Scroll::PageDown,
            b'b' => Scroll::PageUp,
            b'k' => Scroll::Up(1),
            b'j' => Scroll::Down(1),
            b'g' => Scroll::Top,
            b'G' => Scroll::Bottom,
            _ => return None,
        };
        Some(Action::Scroll(scroll))
    }

    /// Open the prompt an action asks for, if it asks for one. The tables above
    /// say what a key means and do nothing, so that they can be asked from
    /// either spelling; this is the one thing that has to happen either way.
    fn opening(&mut self, action: Action) {
        match action {
            Action::Find(Find::Open) => self.open(Prompting::Find),
            Action::Rename(Rename::Open) => self.open(Prompting::Rename),
            _ => {}
        }
    }

    /// Whether the paste key is the client's. Off hands it back to the session,
    /// which is what `MM_PASTE=off` asks for.
    pub fn set_paste(&mut self, on: bool) {
        self.paste = on;
    }

    /// Whether there is a history to look at. Off on a screen the terminal owns
    /// (it has the lines itself) and on a host too old to send a window of one.
    pub fn set_scroll(&mut self, on: bool) {
        self.scroll = on;
        if !on {
            self.wheel = false;
        }
    }

    /// Whether the wheel is the client's right now, which it is only while the
    /// client has tracking on and the session has not asked for any.
    pub fn set_wheel(&mut self, ours: bool) {
        self.wheel = ours && self.scroll;
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
            // A prompt open takes everything, so that a search for `d` is a
            // search and not a detach. The whole chunk at once, because typing
            // arrives in chunks and one action per byte would drop the rest of
            // each of them.
            if self.prompt.is_some() {
                let action = self.typing(&input[i..]);
                self.mode = Self::after(action);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                };
            }
            // The wheel, while it is the client's. Every report is swallowed,
            // not only the ones that move the view: the client asked the
            // terminal for them, the session did not, and forwarding a click
            // it never asked to hear about would type into whatever is
            // running.
            if self.wheel
                && let Some(report) = Report::parse(&input[i..])
            {
                // Every report in the chunk, not just this one. A hand moving
                // the wheel sends several before the client is next read, and
                // stopping at the first would leave the rest to be read as
                // keystrokes and the view a notch behind the hand.
                let mut net = 0i64;
                let mut report = Some(report);
                while let Some(this) = report {
                    i += this.len;
                    net += match this.scroll() {
                        Some(Scroll::Up(lines)) => lines as i64,
                        Some(Scroll::Down(lines)) => -(lines as i64),
                        _ => 0,
                    };
                    report = Report::parse(&input[i..]);
                }
                let scroll = match net {
                    0 => continue,
                    net if net > 0 => Scroll::Up(net as u64),
                    net => Scroll::Down(net.unsigned_abs()),
                };
                let action = Action::Scroll(scroll);
                self.mode = Self::after(action);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                };
            }
            // The keys that drive the view, which are escape sequences of their
            // own and have to be read whole before the escape starting them is
            // taken for the Esc that leaves.
            if self.mode == Mode::Scroll
                && let Some((_, scroll)) = VIEW_KEYS
                    .iter()
                    .find(|(spelling, _)| input[i..].starts_with(spelling))
            {
                let action = Action::Scroll(*scroll);
                self.mode = Self::after(action);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                };
            }
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
            if self.mode == Mode::Scroll {
                // Nothing typed here reaches the session: the screen is showing
                // its history, and a key meant for the program would land in a
                // screen that is not the one being looked at. Unbound keys do
                // nothing rather than dropping out of the view, so a hand on
                // the keyboard cannot lose your place in a long build.
                let Some(action) = self.scrolling(b) else {
                    continue;
                };
                self.opening(action);
                self.mode = Self::after(action);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                };
            }
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
                other => match self.controlling(other) {
                    Some(action) => {
                        self.opening(action);
                        Some(action)
                    }
                    None => {
                        self.mode = Mode::Focus;
                        forward.extend_from_slice(&self.spelling);
                        forward.push(other);
                        None
                    }
                },
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

    /// Open a prompt, empty.
    fn open(&mut self, what: Prompting) {
        self.prompt = Some(Prompt {
            what,
            typed: Vec::new(),
        });
    }

    /// Everything typed at an open prompt, in one go.
    ///
    /// Stops at the first key that ends the typing, and drops whatever followed
    /// it: nobody types through an Enter, the same way nobody types through a
    /// detach. Bytes that are not printable are dropped rather than added to
    /// the line, which keeps an arrow key from becoming three characters of
    /// search text or of title.
    ///
    /// An escape sequence is taken off whole before any of that, because the
    /// Esc it starts with is not the Esc key. A program holding the terminal in
    /// an extended-keys mode makes this the common case rather than a corner:
    /// the ctrl let go of a moment after the key that opened the prompt reports
    /// itself, and read a byte at a time that release closed the prompt before
    /// anything could be typed into it.
    fn typing(&mut self, input: &[u8]) -> Action {
        let Some(prompt) = self.prompt.as_mut() else {
            return Action::Find(Find::Cancel);
        };
        let what = prompt.what;
        let mut i = 0;
        while i < input.len() {
            let mut b = input[i];
            i += 1;
            if b == 0x1b
                && let Some(len) = sequence(&input[i - 1..])
            {
                let key = Encoded::parse(&input[i - 1..]);
                i += len - 1;
                let Some(key) = key else {
                    continue;
                };
                match key.typed() {
                    Some(byte) => b = byte,
                    // A character with no byte of its own, which is how one
                    // arrives once a program has asked for every key as an
                    // escape code. Everything else the mode brings with it
                    // (releases, chords, the keys that type nothing) is not
                    // typing and is dropped.
                    None => {
                        if let Some(typed) = key.text() {
                            let mut spelt = [0; 4];
                            let spelt = typed.encode_utf8(&mut spelt);
                            prompt.typed.extend_from_slice(spelt.as_bytes());
                        }
                        continue;
                    }
                }
            }
            match b {
                b'\r' | b'\n' => return what.action(Find::Run, Rename::Run),
                0x1b => {
                    self.prompt = None;
                    return what.action(Find::Cancel, Rename::Cancel);
                }
                // Backspace, in both spellings terminals send.
                0x08 | 0x7f => {
                    // A rub with nothing left to rub out closes the prompt,
                    // which is where a hand that changed its mind ends up.
                    if pop_char(&mut prompt.typed).is_none() {
                        self.prompt = None;
                        return what.action(Find::Cancel, Rename::Cancel);
                    }
                }
                // Ctrl-U, which is what a shell means by "start again".
                0x15 => prompt.typed.clear(),
                // Printable, and the bytes of a multi-byte character, which
                // are all above 0x7f and go in as they come.
                0x20..=0x7e | 0x80.. => prompt.typed.push(b),
                _ => {}
            }
        }
        what.action(Find::Typed, Rename::Typed)
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
        // The key as the byte the ordinary encoding would have sent, so that
        // the tables the short spelling is read with answer for this one too.
        // The mode key is not among them: it is a chord, and what it means is
        // read from the chord.
        let byte = if key.is(self.prefix) {
            Some(self.prefix)
        } else {
            key.byte()
        };
        if self.mode == Mode::Scroll {
            // Every key in the view is the client's, the ones that mean nothing
            // there included: the view must not drop because a hand landed on
            // the keyboard, and the session behind it must not be typed into.
            let action = byte.and_then(|b| self.scrolling(b))?;
            self.opening(action);
            return Some(action);
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
            ESCAPE | ENTER => {
                self.mode = Mode::Focus;
                return None;
            }
            _ => {}
        }
        if let Some(action) = byte.and_then(|b| self.controlling(b)) {
            self.opening(action);
            return Some(action);
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

    use super::{Action, Find, KeyFilter, Mode, Outcome, Rename, Scroll, wheel_is_ours};
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
        let carries_events = session.events;
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

        // A host from before the attach stream carried events will never send
        // one, and there is no key here to find that out with: the other
        // capabilities go quiet under a keystroke somebody chose to press,
        // while this one goes quiet under a bell nobody was watching for. Said
        // once, on the row, and only to someone who has bells switched on,
        // since silence is what the rest asked for. The first repaint paints
        // it, which is why nothing is written here.
        if !carries_events && notify::to_terminal() {
            status.set_notice("this host is too old to relay bells; `mm restart` there");
            notice_until = Some(tokio::time::Instant::now() + NOTICE_FOR);
            restate = true;
        }

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
            // Nothing here scrolls, so nothing here asked for a window or went
            // looking through one, and nothing here renames.
            Update::View(_) | Update::Found(_) | Update::Renamed(_) => continue,
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

    /// The key opens the view and the wheel moves it from there: the client
    /// asks the terminal for reports only once there is a window for them to
    /// move, so that the rest of the time a drag is a selection.
    #[test]
    fn the_wheel_moves_the_view_the_key_opened() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        f.set_wheel(true);
        assert_eq!(
            f.filter(b"\x1b[<64;10;5M"),
            Keystrokes {
                forward: Vec::new(),
                action: Some(Action::Scroll(Scroll::Up(scroll::WHEEL))),
                mode: Mode::Scroll,
            }
        );
    }

    /// A hand on the wheel sends several reports before the client is next
    /// read. Answering only the first would leave the rest to be read as
    /// keystrokes, and the view one notch behind the hand.
    #[test]
    fn a_chunk_of_wheel_reports_moves_the_view_once_for_all_of_them() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        let three = b"\x1b[<64;1;1M\x1b[<64;1;1M\x1b[<64;1;1M";
        assert_eq!(
            f.filter(three),
            Keystrokes {
                forward: Vec::new(),
                action: Some(Action::Scroll(Scroll::Up(3 * scroll::WHEEL))),
                mode: Mode::Scroll,
            }
        );

        // And a hand that changed its mind mid-chunk moves by the difference.
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(f.filter(b"\x1b[<64;1;1M\x1b[<65;1;1M"), forwarded(b""));
    }

    /// The client asked the terminal for these reports; the session did not.
    /// Forwarding a click it never asked to hear about would type into it.
    #[test]
    fn a_click_the_client_asked_for_is_swallowed_rather_than_forwarded() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(f.filter(b"\x1b[<0;10;5M\x1b[<0;10;5m"), forwarded(b""));
    }

    /// The mouse is the terminal's while you are looking at the live session,
    /// so a drag selects and a double click takes a word, the way they do in
    /// any other program. A client holding mouse reports for the whole attach
    /// is a client you cannot copy a line out of without a modifier held, and
    /// the wheel it holds them for has nothing to scroll until the view is up.
    #[cfg(feature = "desktop")]
    #[test]
    fn the_wheel_is_the_clients_only_while_the_view_is_up() {
        assert!(wheel_is_ours(true, false));
        assert!(!wheel_is_ours(false, false), "nothing to scroll yet");
    }

    /// A program that asked for the mouse keeps it, view or no view. Taking it
    /// would leave two readers on one wheel, and giving it back on the way out
    /// of the view would switch off tracking the client never switched on.
    #[cfg(feature = "desktop")]
    #[test]
    fn the_wheel_is_never_taken_from_a_session_that_asked_for_it() {
        assert!(!wheel_is_ours(true, true));
        assert!(!wheel_is_ours(false, true));
    }

    /// While the session has the mouse, every report is its own and the client
    /// reads none of them: two programs reading one wheel is one of them
    /// reading input meant for the other.
    #[test]
    fn a_session_that_asked_for_the_mouse_keeps_its_reports() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(false);
        let report = b"\x1b[<64;10;5M";
        assert_eq!(f.filter(report), forwarded(report));
    }

    #[test]
    fn the_scroll_key_opens_the_view_and_the_keys_move_it() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        assert_eq!(
            f.filter(&[KEY, SCROLL_KEY]),
            asked(Action::Scroll(Scroll::Up(0)), Mode::Scroll)
        );
        for (keys, scroll) in [
            (&b"\x1b[5~"[..], Scroll::PageUp),
            (&b"\x1b[6~"[..], Scroll::PageDown),
            (&b"\x1b[A"[..], Scroll::Up(1)),
            (&b"g"[..], Scroll::Top),
            (&b"G"[..], Scroll::Bottom),
            (&b" "[..], Scroll::PageDown),
        ] {
            assert_eq!(
                f.filter(keys),
                asked(Action::Scroll(scroll), Mode::Scroll),
                "{keys:?}"
            );
        }
        assert_eq!(
            f.filter(b"\x1b"),
            asked(Action::Scroll(Scroll::Leave), Mode::Focus)
        );
    }

    /// Nothing typed at the view reaches the session: the screen is showing the
    /// history, and a key meant for the program would land somewhere nobody is
    /// looking. An unbound one does nothing rather than dropping out of the
    /// view, so a hand on the keyboard cannot lose your place in a long build.
    #[test]
    fn keys_the_view_does_not_use_go_nowhere() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        assert_eq!(
            f.filter(b"xyz"),
            Keystrokes {
                forward: Vec::new(),
                action: None,
                mode: Mode::Scroll,
            }
        );
    }

    /// Inline the terminal has the history in its own buffer, its own wheel
    /// scrolls it and its own find bar searches it, so neither key is the
    /// client's and both go to the session the way any unbound key does. A host
    /// too old to answer is a different case: the keys stay the client's there,
    /// so that it can say why nothing happened.
    #[test]
    fn neither_scrolling_nor_searching_is_taken_where_the_terminal_owns_the_history() {
        for key in [SCROLL_KEY, FIND_KEY] {
            let mut f = KeyFilter::new(KEY);
            f.set_scroll(false);
            assert_eq!(f.filter(&[KEY, key]), forwarded(&[KEY, key]), "{key}");
            assert_eq!(f.needle(), None, "no prompt was opened either");
        }
    }

    /// And the wheel is left alone there too, so the terminal keeps scrolling
    /// its own buffer rather than the client reading reports nobody asked for.
    #[test]
    fn the_wheel_stays_the_terminals_where_the_terminal_owns_the_history() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(false);
        f.set_wheel(true);
        let report = b"\x1b[<64;10;5M";
        assert_eq!(f.filter(report), forwarded(report));
    }

    /// The whole gesture: `Ctrl-] /`, type, Enter. The needle stays in the
    /// filter until it is run, because a chunk holding several typed bytes has
    /// to become one needle rather than an action per byte.
    #[test]
    fn a_search_is_typed_at_the_prompt_and_run_with_enter() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        assert_eq!(
            f.filter(&[KEY, FIND_KEY]),
            asked(Action::Find(Find::Open), Mode::Scroll)
        );
        assert_eq!(f.needle().as_deref(), Some(""));

        assert_eq!(
            f.filter(b"error"),
            asked(Action::Find(Find::Typed), Mode::Scroll)
        );
        assert_eq!(f.needle().as_deref(), Some("error"));

        assert_eq!(
            f.filter(b"\r"),
            asked(Action::Find(Find::Run), Mode::Scroll)
        );
    }

    /// The reason the prompt takes every key while it is open. `d` detaches
    /// everywhere else, and a search for `docker` starts with one.
    #[test]
    fn keys_that_do_things_elsewhere_are_just_letters_at_the_prompt() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, FIND_KEY]);
        assert_eq!(
            f.filter(b"docker qng"),
            asked(Action::Find(Find::Typed), Mode::Scroll)
        );
        assert_eq!(f.needle().as_deref(), Some("docker qng"));
    }

    #[test]
    fn a_prompt_can_be_rubbed_out_and_backed_out_of() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, FIND_KEY]);
        f.filter(b"abc");
        f.filter(&[0x7f]);
        assert_eq!(f.needle().as_deref(), Some("ab"));
        f.filter(&[0x15]); // Ctrl-U, start again
        assert_eq!(f.needle().as_deref(), Some(""));

        // Rubbing out the last of nothing closes the prompt, which is where a
        // hand that changed its mind ends up.
        assert_eq!(
            f.filter(&[0x7f]),
            asked(Action::Find(Find::Cancel), Mode::Scroll)
        );
        assert_eq!(f.needle(), None);

        f.filter(&[FIND_KEY]);
        assert_eq!(
            f.filter(&[0x1b]),
            asked(Action::Find(Find::Cancel), Mode::Scroll)
        );
        assert_eq!(f.needle(), None);
    }

    /// A needle typed in a language the terminal sends as several bytes each.
    #[test]
    fn a_needle_can_hold_more_than_ascii() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, FIND_KEY]);
        f.filter("Ubicación".as_bytes());
        assert_eq!(f.needle().as_deref(), Some("Ubicación"));
    }

    #[test]
    fn the_matches_are_walked_with_n_and_shift_n() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        assert_eq!(
            f.filter(b"n"),
            asked(Action::Find(Find::Next), Mode::Scroll)
        );
        assert_eq!(
            f.filter(b"N"),
            asked(Action::Find(Find::Previous), Mode::Scroll)
        );
    }

    /// One backspace undoes one keypress, whatever the terminal spent on it.
    /// Popping a byte left half a character behind, which showed on the row as
    /// a replacement character and went to the host as one.
    #[test]
    fn a_rub_takes_a_whole_character_off_however_many_bytes_it_was() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, FIND_KEY]);
        f.filter("día".as_bytes());
        f.filter(&[0x7f]);
        assert_eq!(f.needle().as_deref(), Some("dí"));
        f.filter(&[0x7f]);
        assert_eq!(f.needle().as_deref(), Some("d"));
    }

    /// The whole gesture, the same as a search: `Ctrl-] r`, type, Enter. The
    /// name stays in the filter until it is run, for the same reason the needle
    /// does.
    #[test]
    fn a_name_is_typed_at_the_prompt_and_sent_with_enter() {
        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(&[KEY, b'r']),
            asked(Action::Rename(Rename::Open), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some(""));

        assert_eq!(
            f.filter("nightly-bench".as_bytes()),
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some("nightly-bench"));

        // And back to the session, which is where you were before.
        assert_eq!(
            f.filter(b"\r"),
            asked(Action::Rename(Rename::Run), Mode::Focus)
        );
    }

    /// Nothing typed at the prompt reaches the session, keys that do things
    /// elsewhere included: a name beginning with `d` is a name, not a detach,
    /// and the tab that would walk the sessions is dropped the way every other
    /// unprintable key is.
    #[test]
    fn nothing_typed_at_the_rename_prompt_reaches_the_session() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(b"deploy\tp"),
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some("deployp"));
    }

    /// Enter on an empty prompt is still a rename, sent and refused by the
    /// host: there is no name in it, and the client says so with the sentence
    /// the host sent back rather than deciding for itself.
    #[test]
    fn an_empty_prompt_is_still_sent() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(b"\r"),
            asked(Action::Rename(Rename::Run), Mode::Focus)
        );
        assert_eq!(f.wanted_name().as_deref(), Some(""));
    }

    #[test]
    fn the_rename_prompt_can_be_rubbed_out_and_backed_out_of() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        f.filter(b"abc");
        f.filter(&[0x7f]);
        assert_eq!(f.wanted_name().as_deref(), Some("ab"));

        assert_eq!(
            f.filter(&[0x1b]),
            asked(Action::Rename(Rename::Cancel), Mode::Focus)
        );
        assert_eq!(f.wanted_name(), None);

        // And the rub with nothing left to rub out, which is the other way a
        // hand that changed its mind gets out.
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(&[0x7f]),
            asked(Action::Rename(Rename::Cancel), Mode::Focus)
        );
        assert_eq!(f.wanted_name(), None);
    }

    /// A program that asks the terminal for every key as an escape code takes
    /// the letters with it, and the client's own keys are letters. Read as
    /// unbound they dropped back to focus and landed in the session instead of
    /// doing what they say on the row.
    #[test]
    fn control_mode_reads_the_letters_in_the_long_spelling() {
        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[100u"),
            asked(Action::Detach, Mode::Focus)
        );

        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[114u"),
            asked(Action::Rename(Rename::Open), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some(""));

        // Shift is the alternate the terminal reports beside the key, since the
        // client's own keys read their case: `H` is not `h`, and the two go
        // opposite ways.
        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[104u"),
            asked(Action::Switch(Motion::NextHost), Mode::Control)
        );
        assert_eq!(
            f.filter(b"\x1b[104:72;2u"),
            asked(Action::Switch(Motion::PreviousHost), Mode::Control)
        );
        // And where no alternate was asked for, the case is worked out here.
        assert_eq!(
            f.filter(b"\x1b[104;2u"),
            asked(Action::Switch(Motion::PreviousHost), Mode::Control)
        );
        // A chord is still nobody's key but the session's.
        assert_eq!(
            f.filter(b"\x1b[104;5u"),
            forwarded(b"\x1b[93;5u\x1b[104;5u")
        );
    }

    /// The view is left with Esc, and with a program holding the terminal in an
    /// extended-keys mode the Esc key is `CSI 27 u`. Read as anything else it
    /// left the mode without leaving the view: the client sat showing history
    /// while everything typed went to the session behind it.
    #[test]
    fn the_view_is_driven_in_the_long_spelling_too() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        assert_eq!(
            f.filter(&[KEY, SCROLL_KEY]),
            asked(Action::Scroll(Scroll::Up(0)), Mode::Scroll)
        );
        assert_eq!(
            f.filter(b"\x1b[27u"),
            asked(Action::Scroll(Scroll::Leave), Mode::Focus)
        );

        // The moves and the search read the same way, and a key the view does
        // not bind is swallowed: it neither drops the view nor reaches the
        // session.
        f.filter(&[KEY, SCROLL_KEY]);
        assert_eq!(
            f.filter(b"\x1b[106u"),
            asked(Action::Scroll(Scroll::Down(1)), Mode::Scroll)
        );
        assert_eq!(
            f.filter(b"\x1b[122;5u"),
            Keystrokes {
                forward: vec![],
                action: None,
                mode: Mode::Scroll,
            }
        );
        assert_eq!(
            f.filter(b"\x1b[47u"),
            asked(Action::Find(Find::Open), Mode::Scroll)
        );
        assert_eq!(f.needle().as_deref(), Some(""));

        // And the mode key leaves the view rather than only the mode.
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        assert_eq!(
            f.filter(b"\x1b[93;5u"),
            asked(Action::Scroll(Scroll::Leave), Mode::Focus)
        );
    }

    /// The bug this was written for: with a program like `pi` holding the
    /// terminal in an extended-keys mode, letting go of the ctrl that opened
    /// control mode reports itself as an escape sequence, and it arrives after
    /// the `r` that opened the prompt. Read as typing, its leading Esc closed
    /// the prompt again, so a session running one of those programs could not
    /// be renamed.
    #[test]
    fn letting_go_of_the_mode_key_does_not_close_the_prompt_it_opened() {
        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5ur"),
            asked(Action::Rename(Rename::Open), Mode::Rename)
        );
        // The `]` released, then the ctrl that was held with it.
        assert_eq!(
            f.filter(b"\x1b[93;5:3u\x1b[57442;5:3u"),
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some(""));
        assert_eq!(
            f.filter(b"bench"),
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some("bench"));
    }

    /// The same race at the other prompt, which is typed the same way and so
    /// had the same hole in it.
    #[test]
    fn letting_go_of_the_mode_key_does_not_close_the_search_either() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        assert_eq!(
            f.filter(b"\x1b[93;5u/"),
            asked(Action::Find(Find::Open), Mode::Scroll)
        );
        f.filter(b"\x1b[57442;5:3u");
        assert_eq!(f.needle().as_deref(), Some(""), "the prompt is still open");
        f.filter(b"error");
        assert_eq!(f.needle().as_deref(), Some("error"));
    }

    /// Nothing the terminal sends of its own accord is typing: a mouse report
    /// from a session that asked for tracking, or a focus event, arrives at
    /// whatever prompt happens to be open and must leave it as it was.
    #[test]
    fn a_report_arriving_at_a_prompt_is_not_typed_into_it() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        f.filter(b"bui\x1b[<0;12;7Mld");
        assert_eq!(f.wanted_name().as_deref(), Some("build"));
        // An arrow in the application-keypad spelling, and the terminal saying
        // the window lost the focus and got it back.
        assert_eq!(f.filter(b"\x1bOA\x1b[O\x1b[I"), {
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        });
        assert_eq!(f.wanted_name().as_deref(), Some("build"));
    }

    /// And the keys that do end the typing still end it in the long spelling,
    /// which is the one they arrive in once a program has asked for it: an Esc
    /// that only arrived as `CSI 27 u` would leave a prompt with no way out but
    /// rubbing it empty.
    #[test]
    fn the_prompt_reads_the_long_spelling_of_the_keys_that_end_it() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        f.filter(b"bench");
        // Backspace and Ctrl-U, spelt the long way.
        f.filter(b"\x1b[127u");
        assert_eq!(f.wanted_name().as_deref(), Some("benc"));
        f.filter(b"\x1b[117;5u");
        assert_eq!(f.wanted_name().as_deref(), Some(""));

        f.filter(b"nightly");
        assert_eq!(
            f.filter(b"\x1b[13u"),
            asked(Action::Rename(Rename::Run), Mode::Focus)
        );
        assert_eq!(f.wanted_name().as_deref(), Some("nightly"));

        f.stop_typing();
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(b"\x1b[27u"),
            asked(Action::Rename(Rename::Cancel), Mode::Focus)
        );
        assert_eq!(f.wanted_name(), None);
    }

    /// And a name can be typed at it entirely in the long spelling, which is
    /// what arrives from a program that asked for every key as an escape code.
    /// The release each press brings with it types nothing, or every letter
    /// would land twice.
    #[test]
    fn a_name_can_be_typed_in_the_long_spelling() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        // `p`, `í` and shift-`A`, each with its release behind it.
        f.filter(b"\x1b[112u\x1b[112;1:3u");
        f.filter(b"\x1b[237u\x1b[237;1:3u");
        f.filter(b"\x1b[97:65;2u\x1b[97:65;2:3u");
        assert_eq!(f.wanted_name().as_deref(), Some("píA"));
    }

    /// An Esc that is not the start of anything is still the Esc key, whether
    /// it is the last byte of a chunk or has an alt-chord behind it.
    #[test]
    fn a_bare_escape_at_a_prompt_still_closes_it() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(b"na\x1b"),
            asked(Action::Rename(Rename::Cancel), Mode::Focus)
        );
        f.filter(&[KEY, b'r']);
        assert_eq!(
            f.filter(b"na\x1bb"),
            asked(Action::Rename(Rename::Cancel), Mode::Focus)
        );
    }

    /// Two prompts, one buffer. What is typed at one must never turn up at the
    /// other.
    #[test]
    fn the_two_prompts_do_not_share_what_is_typed_at_them() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, FIND_KEY]);
        f.filter(b"error");
        assert_eq!(f.wanted_name(), None, "a needle is not a name");

        // Running the search leaves the needle for the caller to read, which is
        // what closes the prompt, and then the view is left the usual way.
        f.filter(b"\r");
        f.stop_typing();
        f.set_mode(Mode::Focus);

        f.filter(&[KEY, b'r']);
        assert_eq!(f.needle(), None, "a name is not a needle");
        assert_eq!(f.wanted_name().as_deref(), Some(""), "and it starts empty");
    }

    /// The key is bound whether or not the host takes a rename, so that an old
    /// one can be answered with a sentence. Without that it would be a letter
    /// landing in the shell.
    #[test]
    fn the_rename_key_does_not_wait_to_hear_whether_the_host_takes_one() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(false);
        assert_eq!(
            f.filter(&[KEY, b'r']),
            asked(Action::Rename(Rename::Open), Mode::Rename)
        );
    }

    /// Every sequence the filter reads is looked for at an offset into the
    /// chunk it arrived in, and a chunk can end anywhere: mid-escape, mid
    /// parameter, between the two bytes of a UTF-8 character. None of that may
    /// read past the end of the chunk.
    #[test]
    fn a_sequence_cut_off_by_the_end_of_a_chunk_is_not_read_past() {
        let cut = [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[<",
            b"\x1b[<64",
            b"\x1b[<64;10;5",
            b"\x1b[5",
            b"\x1bO",
            b"\x1b[27;5",
            b"\x1b[Z"[..1].as_ref(),
            "é".as_bytes()[..1].as_ref(),
        ];
        for tail in cut {
            for mode in [Mode::Focus, Mode::Control, Mode::Scroll] {
                let mut f = KeyFilter::new(KEY);
                f.set_scroll(true);
                f.set_wheel(true);
                f.set_mode(mode);
                // Reaching the end of this is the assertion.
                f.filter(tail);
            }
        }
    }

    /// The same, over anything at all: bytes arrive from a terminal, and what a
    /// terminal sends is only ever mostly what its documentation says.
    #[test]
    fn no_chunk_of_bytes_reads_past_the_end_of_itself() {
        // A generator rather than a crate, so the sweep is the same every run
        // and a failure can be reproduced from the seed alone.
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut byte = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            // Weighted towards the bytes sequences are made of, so the sweep
            // spends its time on shapes that nearly parse.
            const LIKELY: &[u8] = b"\x1b[<;OM~muZAB0123456789";
            if seed.is_multiple_of(3) {
                (seed >> 33) as u8
            } else {
                LIKELY[(seed >> 33) as usize % LIKELY.len()]
            }
        };
        for mode in [Mode::Focus, Mode::Control, Mode::Scroll] {
            let mut f = KeyFilter::new(KEY);
            f.set_scroll(true);
            f.set_wheel(true);
            f.set_mode(mode);
            for _ in 0..2000 {
                let chunk: Vec<u8> = (0..16).map(|_| byte()).collect();
                f.filter(&chunk);
            }
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
