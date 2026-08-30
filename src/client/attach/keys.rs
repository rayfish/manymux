//! Reading the client's own keys out of whatever the terminal sent.
//!
//! Terminal-free on purpose, so all of it is unit tested: what goes in is a
//! chunk of bytes off stdin and what comes out is what to forward, what the
//! client was asked for, and which mode the keyboard is in now.
//!
//! The hard part is that one key has several spellings. A program that asks
//! for the kitty keyboard protocol or for xterm's `modifyOtherKeys` changes how
//! the terminal encodes every chord, so the mode key stops arriving as one byte
//! and starts arriving as an escape sequence. [`Encoded`] reads all of them and
//! [`Encoded::byte`] puts each back to the byte the short spelling would have
//! been, so the tables below are written once and cannot answer two ways.

use std::time::{Duration, Instant};

use crate::client::scroll;

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
/// What a terminal reports for a key being held down long enough to repeat.
/// A keystroke like any other: holding a key is how anybody walks a long list.
const REPEAT: u8 = 2;

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
        let end = rest.iter().position(|b| final_byte(*b))?;
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

    /// Whether the key is down: pressed, or held long enough to be repeating.
    ///
    /// Both count, and only letting go does not. Under the protocols that
    /// report event types at all, a held key stops sending the plain byte over
    /// and over and sends repeats instead, so dropping them meant that holding
    /// tab in the list moved the highlight exactly once, but only while a
    /// program like `pi` had the terminal in that mode.
    fn down(&self) -> bool {
        self.event == PRESS || self.event == REPEAT
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
        if !self.down() {
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
        // Letting go of a key types nothing. Holding one down does, over and
        // over, which is what holding a key has always meant and what a
        // terminal not in one of these modes does by sending the byte again.
        if !self.down() || self.mods & !(SHIFT | LOCKS) != 0 {
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
            let end = rest[1..].iter().position(|b| final_byte(*b))?;
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

/// What a key did to a popup.
///
/// Where the highlight is and what it lands on is [`super::super::picker`]'s;
/// this only says which way, the same split [`Scroll`] uses for the view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Up,
    Down,
    /// The next machine in the list, and the one before: a bigger step of the
    /// same gesture, now that there is a list to see it in.
    NextGroup,
    PreviousGroup,
    /// Enter: go to the highlighted session, or take the highlighted group.
    Go,
    /// `1` to `9`: go to the session wearing that digit, without moving the
    /// highlight there first. The same landing Enter makes, reached in one key
    /// rather than in as many presses as the row is rows away.
    Number(u8),
    /// `m`: move the highlighted session into a group, which opens the group
    /// list over this one.
    Move,
    /// `g`: narrow to a group, which opens the same list for the other verb.
    Groups,
    /// Esc: close, changing nothing.
    Cancel,
}

/// What a key pressed in switch mode asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Detach,
    Switch(Motion),
    /// Something happened to the popup control mode puts on the screen.
    Pick(Pick),
    /// The same four states as a rename, for the prompt that names a new group.
    /// A third prompt rather than a third editor: the typing is identical and
    /// only the action carrying it says which prompt it happened at.
    GroupName(Rename),
    /// Start a session on the machine this one is on, and go and sit in it.
    /// Where that is and what it gets called is the caller's, for the same
    /// reason a switch is: this half of the client knows nothing about hosts.
    New,
    /// Send this machine's clipboard to the session, if there is an image on
    /// it. Deciding that is the caller's: this half of the client knows nothing
    /// about clipboards.
    Paste,
    /// Move the view over the session's history, or open or close it. Where it
    /// is and what it shows is [`super::scroll`]'s; this only says which way.
    Scroll(Scroll),
    /// Something happened to the selection being dragged out in the view.
    /// Where the cells are and what text they hold is [`super::scroll`]'s, the
    /// same as everything else about that window.
    Select(Select),
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
    /// Esc: the name stays as it was.
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

/// A drag, in the four things a hand does with a mouse.
///
/// Selecting happens in the view and nowhere else, so a press on the live
/// screen opens it first: that is where the client holds the lines and paints
/// every row itself, and it is the only surface here whose picture stands
/// still. On the live screen the node holds the screen and the client is a pipe
/// between it and the terminal, with no cells of its own to reverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    /// The button went down here: a selection starts, and anything selected
    /// before it goes.
    From(Spot),
    /// Dragged to here with the button still down.
    To(Spot),
    /// Let go. What is selected is what gets copied.
    Done,
    /// A second click in the same place, which takes the word under it.
    Word(Spot),
    /// And a third, which takes the whole line.
    Line(Spot),
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
    /// A group is being chosen, from the list drawn over the session list.
    ///
    /// Its own mode because the keys that act on a session mean nothing here:
    /// `m` from inside this would be moving a move, and `d` would detach out
    /// of a gesture halfway through.
    Picking,
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
    /// A press. A release ends in `m` instead, and for the wheel there is no
    /// such thing: a notch is one report and reading its halves would move the
    /// view twice.
    press: bool,
    /// The pointer moved with a button held, which is what a drag is made of
    /// and what `?1002h` adds to `?1000h`.
    motion: bool,
    /// Where on the screen, in cells, counting from one.
    at: Spot,
}

/// Buttons 64 and 65, once the modifier bits (shift 4, meta 8, ctrl 16) and the
/// motion bit are taken off. 66 and 67 are the horizontal wheel, which has
/// nowhere to go here.
const WHEEL_UP: u8 = 64;
const WHEEL_DOWN: u8 = 65;
/// The button a selection is made with. The other two are swallowed: the client
/// asked for these reports and the session did not, so there is nowhere to
/// forward them to.
const LEFT: u8 = 0;
const MODIFIERS: u8 = 4 | 8 | 16;
/// Motion, reported with whichever button is down.
const MOTION: u8 = 32;

/// A cell on the screen, counting from one, the way a terminal reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spot {
    pub col: u16,
    pub row: u16,
}

impl Report {
    fn parse(input: &[u8]) -> Option<Self> {
        let rest = input.strip_prefix(b"\x1b[<")?;
        let end = rest.iter().position(|b| matches!(b, b'M' | b'm'))?;
        let mut fields = std::str::from_utf8(&rest[..end]).ok()?.split(';');
        let button = fields.next()?.parse::<u16>().ok()?;
        let button = u8::try_from(button).ok()?;
        // A report with no coordinates is not one. They are the whole of what a
        // drag says, and a zero would be a cell that does not exist.
        let col = fields.next()?.parse::<u16>().ok()?;
        let row = fields.next()?.parse::<u16>().ok()?;
        Some(Self {
            len: 3 + end + 1,
            button: button & !(MODIFIERS | MOTION),
            press: rest[end] == b'M',
            motion: button & MOTION != 0,
            at: Spot { col, row },
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
/// The terminal's unless somebody asked for it here (`mm config mouse client`),
/// and then only where there is a history of our own to move: a screen the
/// client owns, and a host new enough to answer for a window. Both of those
/// arrive inside `history`, along with the setting, since a wheel with nowhere
/// to go and a wheel nobody asked for come to the same thing.
///
/// Taking it is the whole attach or none of it. It was once held only while the
/// view was already up, on the reasoning that a wheel is worth less than a drag,
/// which left the wheel doing nothing at all for the rest of the attach: the
/// client's screen is the terminal's alternate one, where the terminal has no
/// scrollback to offer, and [`crate::client::screen::Alternate`] switches
/// alternate scroll off besides, so a notch reached nobody. A key that only
/// works after you have pressed another key is not the gesture anybody reaches
/// for.
///
/// What taking it costs is the terminal's own selection, which a terminal stops
/// doing with a bare drag the moment somebody is reporting the mouse. Selection
/// moves under a modifier the terminal chooses, which may not be there at all,
/// and that is what settles the default rather than any weighing up of the two
/// gestures: a wheel that does nothing here is what plain ssh does and there is
/// a key on the hints row saying so, while a drag that stopped selecting is
/// this having quietly taken something away, with nothing to press instead.
///
/// Never while the session has asked for reports of its own, which is the half
/// of the old rule that stays: two readers on one wheel, and a program that
/// asked for the mouse must keep every report, including the wheel it draws its
/// own scrolling from.
///
/// Desktop-only, like the terminal it is a rule about: a mobile app has no
/// wheel to route and its own idea of what a drag is.
#[cfg(feature = "desktop")]
pub(super) fn wheel_is_ours(history: bool, session_mouse: bool) -> bool {
    history && !session_mouse
}

/// Shift-Tab, spelt the way a terminal sends it when nothing has asked for a
/// longer encoding. Named because it is the one control sequence reaching a
/// mode the client is holding that a hand pressed, rather than one the terminal
/// sent of its own accord.
const SHIFT_TAB: &[u8] = b"\x1b[Z";

/// The keys that move a popup's highlight, each in the plain spelling a
/// terminal sends it in when nothing has asked for a longer one.
///
/// Matched through [`Special`], which reads the longer spellings back to these,
/// and matched before the bytes are looked at one at a time, for the same reason
/// [`VIEW_KEYS`] is: read a byte at a time the Esc starting `\x1b[A` is the Esc
/// that closes the popup, so reaching for a cursor key would shut the list.
const PICK_KEYS: &[(&[u8], Pick)] = &[
    (b"\x1b[A", Pick::Up),
    (b"\x1b[B", Pick::Down),
    // Shift-Tab, which is spelt like a report and pressed like a key. Here
    // rather than only in the control-mode match, so it moves the group list
    // too: read a byte at a time there, its Esc is the Esc that closes.
    (SHIFT_TAB, Pick::Up),
];

/// The keys that move the view, each in the plain spelling a terminal sends it
/// in when nothing has asked for a longer one.
///
/// Read through [`Special`] and matched before the bytes are looked at one at a
/// time, so that the escape starting each of them is not mistaken for the bare
/// Esc that leaves.
const VIEW_KEYS: &[(&[u8], Scroll)] = &[
    (b"\x1b[5~", Scroll::PageUp),
    (b"\x1b[6~", Scroll::PageDown),
    (b"\x1b[A", Scroll::Up(1)),
    (b"\x1b[B", Scroll::Down(1)),
    (b"\x1b[H", Scroll::Top),
    (b"\x1b[1~", Scroll::Top),
    (b"\x1b[7~", Scroll::Top),
    (b"\x1b[F", Scroll::Bottom),
    (b"\x1b[4~", Scroll::Bottom),
    (b"\x1b[8~", Scroll::Bottom),
];

/// A key that types nothing, as a terminal spells it, with everything an
/// extended-keys mode adds to the spelling taken back off.
///
/// The arrows and the paging keys are the keys [`Encoded`] cannot speak for:
/// they are escape sequences rather than bytes, and they end in a letter or a
/// `~` rather than the `u` that protocol uses. So they are matched against the
/// tables above, and matching the plain spelling alone missed exactly what
/// [`Encoded::down`] once missed. A terminal asked to report event types stops
/// resending `\x1b[A` for a held arrow and sends `\x1b[1;1:2A`, so holding one
/// in the popup moved the highlight once and stopped, but only while a program
/// like `pi` had the terminal in that mode: the same hand on the same key
/// worked in every other session.
///
/// Modifiers are read off and then ignored. Ctrl-Up in a list is a hand
/// reaching for up, and there is nothing else here for it to mean.
struct Special {
    /// How many bytes of the input the sequence took.
    len: usize,
    /// The number in front, which names the key in the `~` spellings and is 1
    /// for the ones a final letter names.
    number: u32,
    /// The final byte: `A` for up, `~` for the numbered keys.
    ends: u8,
    /// 1 press, 2 repeat, 3 release, as [`Encoded`] reads it.
    event: u8,
}

impl Special {
    /// Read one such key off the front of `input`, if what is there is one.
    fn parse(input: &[u8]) -> Option<Self> {
        // SS3, which is how a terminal in application-keypad mode spells the
        // arrows. It carries no parameters at all, so there is nothing an
        // extended mode could have added to it.
        if let Some(rest) = input.strip_prefix(b"\x1bO") {
            let ends = *rest.first()?;
            return final_byte(ends).then_some(Self {
                len: 3,
                number: 1,
                ends,
                event: PRESS,
            });
        }
        let rest = input.strip_prefix(b"\x1b[")?;
        let end = rest.iter().position(|b| final_byte(*b))?;
        let mut fields = std::str::from_utf8(&rest[..end]).ok()?.split(';');
        // An absent number is 1, which is what the protocol says a missing
        // parameter means and what makes `\x1b[A` and `\x1b[1;5A` one key.
        let number = match fields.next() {
            None | Some("") => 1,
            Some(field) => number(field)?,
        };
        let (_, event) = modifiers(fields.next());
        Some(Self {
            len: 2 + end + 1,
            number,
            ends: rest[end],
            event,
        })
    }

    /// Whether this is the key `plain` names, held down rather than let go of.
    fn is(&self, plain: &[u8]) -> bool {
        let Some(named) = Self::parse(plain) else {
            return false;
        };
        (self.event == PRESS || self.event == REPEAT)
            && self.number == named.number
            && self.ends == named.ends
    }
}

/// Whether this byte ends a control sequence.
fn final_byte(b: u8) -> bool {
    (0x40..=0x7e).contains(&b)
}

/// Watches the keystroke stream for the key that changes mode, and reads the
/// keys that follow it.
///
/// Control mode stays on: one mode key then `tab tab tab` walks through the
/// sessions on the machine you are on, and `h` is how you change machine.
/// `Esc`, `Enter` or the mode key goes back to focus, `n` starts a session
/// here and lands you in it, `d` detaches,
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
    /// The last press: where, when, and how many have landed there in a row.
    ///
    /// A double click is a fact about the hand and the clock rather than about
    /// the screen, so it is counted here and the view is told what it means.
    clicked: Option<(Spot, Instant, u8)>,
    /// What is being typed at a prompt, while one is open, and which prompt
    /// it is.
    ///
    /// Kept here rather than by the caller because it is keyboard state, and
    /// because a chunk holding several typed bytes has to become one line
    /// rather than one action per byte, which is a shape this returns nothing
    /// for.
    prompt: Option<Prompt>,
    /// The key code of the press the client last took for itself in a mode of
    /// its own, while that key is still down.
    ///
    /// It is here for the release that follows, which arrives after the action
    /// has usually handed the keyboard back: `Ctrl-] n` starts a session and
    /// lands you in it, so the `n` coming up is read in focus mode and was
    /// forwarded, typing `0;1:3u` into a shell that had just started. The
    /// press it belongs to never reached the session and neither may this.
    /// One slot is enough because only the key that ended the mode can have
    /// its release read anywhere else: everything pressed before it is let go
    /// of while the client still owns the keyboard, where [`Encoded::down`]
    /// drops it.
    acted: Option<u32>,
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
    /// A group to put the highlighted session in.
    Group,
}

impl Prompting {
    /// The same four things happen at every prompt; which action carries them
    /// is all that differs, so the set is named at the one place it matters.
    fn action(self, find: Find, rename: Rename) -> Action {
        match self {
            Prompting::Find => Action::Find(find),
            Prompting::Rename => Action::Rename(rename),
            Prompting::Group => Action::GroupName(rename),
        }
    }
}

/// What is left of a chunk once an action has been taken out of it.
///
/// Only a popup move gives any of it back. See [`Keystrokes::rest`].
fn rest_after(action: Action, left: &[u8]) -> Vec<u8> {
    match action {
        Action::Pick(Pick::Up | Pick::Down | Pick::NextGroup | Pick::PreviousGroup) => {
            left.to_vec()
        }
        _ => Vec::new(),
    }
}

/// How far a wheel report moves the view, signed, for adding up a spin.
fn lines(scroll: Scroll) -> i64 {
    match scroll {
        Scroll::Up(lines) => lines as i64,
        Scroll::Down(lines) => -(lines as i64),
        _ => 0,
    }
}

/// How long after a click a second one in the same place is a double rather
/// than two singles. What every toolkit uses, give or take.
const CLICK_AGAIN: Duration = Duration::from_millis(400);

/// Take the last character off a line being typed, if there is one.
///
/// A character rather than a byte, because one press of a key that put three
/// bytes in has to take all three out again. UTF-8 continuation bytes are
/// `0b10xxxxxx`, so the character starts at the last byte that is not one.
fn pop_char(typed: &mut Vec<u8>) {
    if let Some(start) = typed.iter().rposition(|b| b & 0xc0 != 0x80) {
        typed.truncate(start);
    }
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
    /// What was left of the chunk, for the two things you do carry on through:
    /// moving a popup's highlight, and anything the mouse did.
    ///
    /// Everything else ends the chunk it was found in, because nobody types
    /// through a detach or a switch. A highlight move is not like those: two
    /// writes a moment apart arrive as one read, so `tab` then `Enter` is one
    /// chunk, and dropping the rest there loses the keystroke that commits. The
    /// mouse is the same the whole time rather than now and then, since one
    /// gesture is many reports and they arrive together.
    /// Empty for every other action, which keeps that rule where it was.
    pub rest: Vec<u8>,
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
            clicked: None,
            prompt: None,
            acted: None,
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

    /// What is being typed at the new-group prompt, if that is the one open.
    pub fn wanted_group(&self) -> Option<String> {
        self.typed(Prompting::Group)
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
    /// `now` is the mode the key arrived in, because one action's answer
    /// depends on it: moving a highlight stays in whichever list is showing.
    /// Reading it as control mode meant the key after a move in the group list
    /// was read with the session list's table, where `m` moves and `d`
    /// detaches, which is the whole reason that mode is separate.
    fn after(action: Action, now: Mode) -> Mode {
        match action {
            // A new session is a fresh shell waiting to be typed into, which
            // is why it is the one key here that does not leave the mode on:
            // what follows a hop is often another hop, what follows this is
            // a command.
            Action::Detach | Action::Paste | Action::New => Mode::Focus,
            Action::Switch(_) => Mode::Control,
            // The popup stays up while the highlight is moving, and both ways
            // out of it go back to the session.
            // A digit lands the same way Enter does, and for the same reason
            // leaves the mode: what follows arriving somewhere is typing.
            Action::Pick(Pick::Go | Pick::Number(_)) => Mode::Focus,
            // Except that a group list was opened *over* the session list, so
            // the way out of one is back to the other: the hints on both of
            // them say `esc` and mean it. Told apart by the mode the key
            // arrived in, which is what the group lists have of their own.
            Action::Pick(Pick::Cancel) if now == Mode::Picking => Mode::Control,
            Action::Pick(Pick::Cancel) => Mode::Focus,
            Action::Pick(Pick::Move | Pick::Groups) => Mode::Picking,
            Action::Pick(_) => now,
            // Back to the group list rather than to the session: naming a group
            // was one step of choosing one, and the list it was chosen from is
            // still what you are looking at.
            Action::GroupName(Rename::Run | Rename::Cancel) => Mode::Picking,
            Action::GroupName(_) => Mode::Rename,
            Action::Scroll(Scroll::Leave) => Mode::Focus,
            // Every other move keeps the view up, wherever it was opened from:
            // a wheel notch back in focus mode opens it and stays. Only back:
            // a notch down there is dropped in `mousing` and never arrives.
            Action::Scroll(_) => Mode::Scroll,
            // A search is something you do to the view, so it puts you in it
            // and leaves you there. Cancelling the prompt is cancelling the
            // prompt, not leaving.
            Action::Find(_) => Mode::Scroll,
            // And so is a drag: the press that starts one opens the view under
            // it, and letting go leaves you looking at what you selected rather
            // than back in a session that has moved on underneath.
            Action::Select(_) => Mode::Scroll,
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
            // Tab and shift-tab are the whole of walking the sessions, rather
            // than a letter each beside them: one gesture with a direction is
            // less to remember than two keys, and it leaves the letters for
            // the things done to a session rather than the moves between them.
            //
            // They move the highlight rather than hopping outright, now that
            // there is a list to move it in. Walking three sessions used to be
            // three detaches and three reattaches, over ssh, to see what each
            // one was; it is now one, on the Enter that commits.
            b'\t' | b'j' | b'J' => Action::Pick(Pick::Down),
            b'k' | b'K' => Action::Pick(Pick::Up),
            b'\r' | b'\n' => Action::Pick(Pick::Go),
            // The digits, which are the sessions you have been in this run,
            // most recent first: `1` is this one and `2` is the one you came
            // from, so going back is one key wherever you have walked to. Zero
            // is not among them, there being no zeroth session, and it goes to
            // the session like any other unbound key.
            b'1'..=b'9' => Action::Pick(Pick::Number(b - b'0')),
            // Move the highlighted session into a group, and narrow to one.
            // Two verbs, and which list you came from is what says which:
            // no row in either means two things.
            b'm' | b'M' => Action::Pick(Pick::Move),
            b'g' => Action::Pick(Pick::Groups),
            b'l' | b'L' => Action::Switch(Motion::Last),
            b'n' | b'N' => Action::New,
            // The one key that reads its own case, because shift already means
            // backwards here: `H` is to `h` what shift-tab is to tab, rather
            // than a second letter to remember.
            b'h' => Action::Pick(Pick::NextGroup),
            b'H' => Action::Pick(Pick::PreviousGroup),
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

    /// The same, for the group list drawn over the session list.
    ///
    /// A table of its own rather than a subset of [`Self::controlling`]: the
    /// keys that act on a session have nothing to act on here, and letting one
    /// through would run it against whatever was highlighted behind this list.
    /// Everything not here does nothing, the same as in the view: a hand
    /// landing on the keyboard must not close a gesture halfway through.
    fn picking(&self, b: u8) -> Option<Action> {
        let pick = match b {
            // The same gesture as in the session list: one key with a
            // direction, rather than a second pair of keys to learn for a list
            // that behaves identically.
            b'\t' => Pick::Down,
            b'j' | b'J' => Pick::Down,
            b'k' | b'K' => Pick::Up,
            b'\r' | b'\n' => Pick::Go,
            b'q' | 0x1b => Pick::Cancel,
            b if b == self.prefix => Pick::Cancel,
            // A group that does not exist yet, named at the prompt. It assigns
            // as well as creates, necessarily: a group is a set of live
            // sessions, so an empty one cannot exist and creating one is the
            // same act as putting the first session in it.
            b'n' | b'N' => return Some(Action::GroupName(Rename::Open)),
            _ => return None,
        };
        Some(Action::Pick(pick))
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

    /// What the mouse just did, out of however many reports of it are in this
    /// chunk.
    ///
    /// Every report is swallowed whether or not it means anything here: the
    /// client asked the terminal for them and the session did not, so a click
    /// forwarded on would be typed into whatever is running.
    ///
    /// Both kinds coalesce, and for the same reason. A hand spinning the wheel
    /// and a hand dragging both send several reports before the client is next
    /// read, and answering only the first would leave the rest to be read as
    /// keystrokes and the screen a gesture behind the hand. A wheel adds its
    /// notches up; a drag has no sum, only a latest, so the ones behind it are
    /// dropped. Neither runs past a report of the other kind, which is why the
    /// remainder goes back through [`rest_after`]: a drag and the release that
    /// ends it arrive as one chunk more often than not.
    fn mousing(&mut self, input: &[u8], i: &mut usize, now: Instant) -> Option<Action> {
        let first = Report::parse(&input[*i..])?;
        *i += first.len;
        if let Some(scroll) = first.scroll() {
            let mut net = lines(scroll);
            while let Some(next) = Report::parse(&input[*i..]) {
                let Some(scroll) = next.scroll() else { break };
                *i += next.len;
                net += lines(scroll);
            }
            return match net {
                0 => None,
                net if net > 0 => Some(Action::Scroll(Scroll::Up(net as u64))),
                // Down with no view up is the wheel spinning past the live
                // screen, which is already what is being looked at. Answered
                // it costs a round trip a notch: the view opens, lands at the
                // bottom in the same breath, and closing it asks the node for
                // the screen again. A hand that scrolled back a little and
                // then spun down hard sends far more notches than it went up,
                // and every one past the bottom held the keyboard for the time
                // that repaint took. Nothing to look at is nothing to move.
                _ if self.mode == Mode::Focus => None,
                net => Some(Action::Scroll(Scroll::Down(net.unsigned_abs()))),
            };
        }
        if first.button != LEFT {
            return None;
        }
        if first.motion {
            let mut at = first.at;
            while let Some(next) = Report::parse(&input[*i..]) {
                if !next.motion || next.button != LEFT {
                    break;
                }
                *i += next.len;
                at = next.at;
            }
            return Some(Action::Select(Select::To(at)));
        }
        if !first.press {
            return Some(Action::Select(Select::Done));
        }
        // A press, and the second and third in the same place mean more than
        // the first. Counted here rather than in the view because it is a fact
        // about the hand and the clock, and the view knows about neither.
        let clicks = match self.clicked {
            Some((was, at, clicks)) if was == first.at && now.duration_since(at) < CLICK_AGAIN => {
                clicks.saturating_add(1)
            }
            _ => 1,
        };
        self.clicked = Some((first.at, now, clicks));
        Some(Action::Select(match clicks {
            1 => Select::From(first.at),
            2 => Select::Word(first.at),
            _ => Select::Line(first.at),
        }))
    }

    /// Open the prompt an action asks for, if it asks for one. The tables above
    /// say what a key means and do nothing, so that they can be asked from
    /// either spelling; this is the one thing that has to happen either way.
    fn opening(&mut self, action: Action) {
        match action {
            Action::Find(Find::Open) => self.open(Prompting::Find),
            Action::Rename(Rename::Open) => self.open(Prompting::Rename),
            Action::GroupName(Rename::Open) => self.open(Prompting::Group),
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
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: Vec::new(),
                };
            }
            // The wheel, while it is the client's. Every report is swallowed,
            // not only the ones that move the view: the client asked the
            // terminal for them, the session did not, and forwarding a click
            // it never asked to hear about would type into whatever is
            // running.
            if self.wheel && Report::parse(&input[i..]).is_some() {
                let Some(action) = self.mousing(input, &mut i, now) else {
                    continue;
                };
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    // Whatever is left of the chunk, always: a hand does one
                    // thing at a time and the reports of it arrive together, so
                    // `mousing` stops at the first report that means something
                    // else and this is what hands that one on. A spin and the
                    // click that follows it, or a drag and the release that
                    // ends it, are one read more often than not.
                    rest: input[i..].to_vec(),
                };
            }
            // The keys that drive the view, which are escape sequences of their
            // own and have to be read whole before the escape starting them is
            // taken for the Esc that leaves.
            if self.mode == Mode::Scroll
                && let Some(key) = Special::parse(&input[i..])
                && let Some((_, scroll)) = VIEW_KEYS.iter().find(|(plain, _)| key.is(plain))
            {
                let action = Action::Scroll(*scroll);
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: Vec::new(),
                };
            }
            // The same, for the arrows that move a popup's highlight.
            if matches!(self.mode, Mode::Control | Mode::Picking)
                && let Some(key) = Special::parse(&input[i..])
                && let Some((_, pick)) = PICK_KEYS.iter().find(|(plain, _)| key.is(plain))
            {
                self.spell(&input[i..i + key.len]);
                self.pressed = None;
                i += key.len;
                let action = Action::Pick(*pick);
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: rest_after(action, &input[i..]),
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
                    self.mode = Self::after(action, self.mode);
                    return Keystrokes {
                        forward,
                        action: Some(action),
                        mode: self.mode,
                        rest: rest_after(action, &input[i..]),
                    };
                }
                continue;
            }
            // A sequence the terminal sent of its own accord rather than one a
            // hand pressed: a mouse report from a session that asked for
            // tracking, a focus event as the window is tabbed away from and
            // back. Read a byte at a time the Esc in front of it is the Esc key,
            // so moving the mouse dropped control mode and the rest of the
            // report was typed into the session behind it. A mode the client is
            // holding is left only by something the user typed; in focus mode
            // these are the session's and go straight through, which is why
            // that mode is not here.
            if self.mode != Mode::Focus
                && !input[i..].starts_with(SHIFT_TAB)
                && let Some(len) = sequence(&input[i..])
            {
                i += len;
                continue;
            }
            let b = input[i];
            i += 1;
            if self.mode == Mode::Picking {
                // Same rule as the view: nothing here reaches the session, and
                // an unbound key does nothing rather than closing the list.
                let Some(action) = self.picking(b) else {
                    continue;
                };
                self.opening(action);
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: rest_after(action, &input[i..]),
                };
            }
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
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: Vec::new(),
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
                        rest: Vec::new(),
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
                // Closes the popup, and sends the byte on when it was pressed
                // twice quickly. An action rather than a bare mode change,
                // because the popup has to be taken off the screen and only
                // the caller can repaint what was under it.
                b if b == self.prefix => {
                    self.spell(&[b]);
                    if pressed.is_some_and(|at| now.duration_since(at) < LITERAL) {
                        forward.push(b);
                    }
                    self.mode = Mode::Focus;
                    None
                }
                // Shift-Tab, which starts with the same byte as the Esc that
                // closes the popup. An Esc with `[Z` behind it in the same
                // read is the key; one at the end of a read is a real Esc.
                // Split across two reads it reads as an Esc, which costs a trip
                // back to focus and nothing else.
                0x1b if input[i..].starts_with(&SHIFT_TAB[1..]) => {
                    i += 2;
                    Some(Action::Pick(Pick::Up))
                }
                0x1b | b'\n' => {
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
            // detach or a switch. A popup move is the exception, and hands the
            // rest back; see `rest_after`.
            if let Some(action) = action {
                self.mode = Self::after(action, self.mode);
                return Keystrokes {
                    forward,
                    action: Some(action),
                    mode: self.mode,
                    rest: rest_after(action, &input[i..]),
                };
            }
        }
        Keystrokes {
            forward,
            action: None,
            mode: self.mode,
            rest: Vec::new(),
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
                // Backspace, in both spellings terminals send. A rub with
                // nothing left to rub out does nothing: rubbing a line out to
                // start it again is how a name gets retyped, and closing the
                // prompt on the last one threw away the gesture halfway
                // through. Esc is the way out, and it is the only one.
                0x08 | 0x7f => pop_char(&mut prompt.typed),
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
                // belong to never got there either. Three keys are the
                // client's, and the third is the one this mode cannot see for
                // itself: a key control mode acted on, since the action that
                // ran is usually the one that handed the keyboard back, and by
                // the time the hand comes off the key this is where it is read.
                let ours = key.is(self.prefix)
                    || (self.paste && key.is(PASTE_KEY))
                    || self.acted == Some(key.code);
                if !ours {
                    forward.extend_from_slice(spelling);
                }
                // A repeat is the key still being held, so it stays the
                // client's until it is let go of: forgetting it on the first
                // repeat leaked every one after it, which is a key held down
                // rather than a key pressed and is exactly the case that fills
                // a line with them.
                if key.event != REPEAT && self.acted == Some(key.code) {
                    self.acted = None;
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

        // Dropping the releases is what makes control mode usable at all once a
        // program has asked for event types: the ctrl you were holding reports
        // its own release the moment you let go of the mode key, and reading
        // that as a key would drop you back to focus before you had typed
        // anything. A repeat is not one of those. It is the key still being
        // held, which is how a long list gets walked, and dropping it meant
        // holding tab moved the highlight once and then stopped.
        if !key.down() || key.is_modifier() {
            return None;
        }
        // From here the press is the client's: every path below either acts on
        // it or drops it, and the one that hands it to the session after all
        // says so by clearing this again. Set before the tables rather than at
        // each of them, since what has to be remembered is the same for all of
        // them and a table that forgot would leak a release into a session.
        self.acted = Some(key.code);
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
        if self.mode == Mode::Picking {
            // The same, for the group list, Shift-Tab included: it walks the
            // list back, and a terminal asked for a protocol that can say which
            // key was held spells it as tab with shift rather than as `CSI Z`,
            // where reading the byte behind it walks the list the other way.
            if key.code == TAB && key.mods & SHIFT != 0 {
                return Some(Action::Pick(Pick::Up));
            }
            let action = byte.and_then(|b| self.picking(b))?;
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
            TAB if key.mods & SHIFT != 0 => return Some(Action::Pick(Pick::Up)),
            ESCAPE => {
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
        // The press went to the session, so its release belongs there too.
        self.mode = Mode::Focus;
        self.acted = None;
        forward.extend_from_slice(&self.spelling);
        forward.extend_from_slice(spelling);
        None
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
            rest: Vec::new(),
        }
    }

    /// A key that took the keyboard into control mode without asking for
    /// anything or reaching the session.
    fn held() -> Keystrokes {
        Keystrokes {
            forward: vec![],
            action: None,
            mode: Mode::Control,
            rest: Vec::new(),
        }
    }

    /// A key that asked for something, with nothing forwarded alongside it.
    fn asked(action: Action, mode: Mode) -> Keystrokes {
        Keystrokes {
            forward: vec![],
            action: Some(action),
            mode,
            rest: Vec::new(),
        }
    }

    /// The mode key, whatever it is. Named because a control byte in the middle
    /// of a byte string is unreadable.
    const KEY: u8 = DEFAULT_PREFIX;
    /// A notch with no view up opens one, which is the whole point of holding
    /// the wheel for the attach rather than for the view: the gesture everybody
    /// reaches for first is the wheel, and it cannot open what it is not being
    /// reported for.
    #[test]
    fn a_notch_on_the_live_session_opens_the_view() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(f.mode, Mode::Focus, "the view is not up");
        assert_eq!(
            f.filter(b"\x1b[<64;10;5M"),
            Keystrokes {
                forward: Vec::new(),
                action: Some(Action::Scroll(Scroll::Up(scroll::WHEEL))),
                mode: Mode::Scroll,
                rest: Vec::new(),
            }
        );
    }

    /// Down there is nothing to open: the live screen is the bottom, and the
    /// notch that would land on it lands on it already. Answering it opens a
    /// view and shuts it again, which asks the node for the screen once per
    /// notch, so a hand that spun down hard after a short look back held the
    /// keyboard for a repaint a notch until the spin ran out.
    #[test]
    fn a_notch_down_on_the_live_session_does_nothing_at_all() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(f.filter(b"\x1b[<65;10;5M"), forwarded(b""));
        assert_eq!(f.mode, Mode::Focus, "and the keyboard stays the session's");

        // A spin of them, the way one arrives, and one that changed its mind
        // on the way and is still going down.
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        let down = b"\x1b[<65;1;1M\x1b[<65;1;1M\x1b[<64;1;1M\x1b[<65;1;1M";
        assert_eq!(f.filter(down), forwarded(b""));
        assert_eq!(f.mode, Mode::Focus);
    }

    /// Inside the view it moves as it always did: down there is a window with
    /// somewhere to go, and the client at the bottom is what closes it.
    #[test]
    fn a_notch_down_inside_the_view_still_moves_it() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        f.set_wheel(true);
        assert_eq!(
            f.filter(b"\x1b[<65;10;5M"),
            Keystrokes {
                forward: Vec::new(),
                action: Some(Action::Scroll(Scroll::Down(scroll::WHEEL))),
                mode: Mode::Scroll,
                rest: Vec::new(),
            }
        );
    }

    /// And the key still opens it, for a hand that would rather not reach for
    /// the mouse and for a terminal reporting nothing.
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
                rest: Vec::new(),
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
                rest: Vec::new(),
            }
        );

        // And a hand that changed its mind mid-chunk moves by the difference.
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(f.filter(b"\x1b[<64;1;1M\x1b[<65;1;1M"), forwarded(b""));
    }

    /// The client asked the terminal for these reports; the session did not.
    /// Forwarding one it never asked to hear about would type into it, whether
    /// or not the client had anything to do with it.
    #[test]
    fn nothing_the_mouse_does_is_forwarded_to_the_session() {
        for reports in [
            &b"\x1b[<0;10;5M"[..],  // a press
            &b"\x1b[<32;10;5M"[..], // dragged
            &b"\x1b[<0;10;5m"[..],  // let go
            &b"\x1b[<1;10;5M"[..],  // the middle button, which means nothing here
            &b"\x1b[<2;10;5M"[..],  // nor the right
            &b"\x1b[<64;1;1M"[..],  // and the wheel
        ] {
            let mut f = KeyFilter::new(KEY);
            f.set_scroll(true);
            f.set_wheel(true);
            assert!(
                f.filter(reports).forward.is_empty(),
                "{}",
                String::from_utf8_lossy(reports)
            );
        }
    }

    /// The three halves of a drag, in the spelling `?1002h` reports them in.
    #[test]
    fn a_drag_is_a_press_a_move_and_a_release() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        assert_eq!(
            f.filter(b"\x1b[<0;10;5M").action,
            Some(Action::Select(Select::From(Spot { col: 10, row: 5 })))
        );
        assert_eq!(
            f.filter(b"\x1b[<32;14;7M").action,
            Some(Action::Select(Select::To(Spot { col: 14, row: 7 })))
        );
        let done = f.filter(b"\x1b[<0;14;7m");
        assert_eq!(done.action, Some(Action::Select(Select::Done)));
        assert_eq!(done.mode, Mode::Scroll, "a drag opens the view under it");
    }

    /// A hand moving sends a report per cell, and they arrive in one read. Only
    /// the latest says anything, and the release behind them has to survive the
    /// chunk: dropped, the button would still be down as far as this is
    /// concerned.
    #[test]
    fn a_dragging_hand_is_read_as_where_it_got_to_and_then_let_go() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        f.filter(b"\x1b[<0;1;1M");
        let moved = f.filter(b"\x1b[<32;2;1M\x1b[<32;3;1M\x1b[<32;9;4M\x1b[<0;9;4m");
        assert_eq!(
            moved.action,
            Some(Action::Select(Select::To(Spot { col: 9, row: 4 })))
        );
        assert_eq!(
            f.filter(&moved.rest).action,
            Some(Action::Select(Select::Done)),
            "the release was handed back rather than eaten"
        );
    }

    /// A wheel spin is still added up, and a report of the other kind ends the
    /// run rather than being swallowed into it.
    #[test]
    fn a_spin_stops_at_the_press_that_follows_it() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        let spun = f.filter(b"\x1b[<64;1;1M\x1b[<64;1;1M\x1b[<0;3;2M");
        assert_eq!(
            spun.action,
            Some(Action::Scroll(Scroll::Up(2 * scroll::WHEEL)))
        );
        assert_eq!(
            f.filter(&spun.rest).action,
            Some(Action::Select(Select::From(Spot { col: 3, row: 2 })))
        );
    }

    /// Two clicks in the same place inside the window are a double click, and
    /// three are a triple. Further apart in time, or in place, they are clicks.
    #[test]
    fn clicking_twice_takes_a_word_and_three_times_takes_the_line() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.set_wheel(true);
        let at = Instant::now();
        let spot = Spot { col: 4, row: 2 };
        assert_eq!(
            f.filter_at(b"\x1b[<0;4;2M", at).action,
            Some(Action::Select(Select::From(spot)))
        );
        assert_eq!(
            f.filter_at(b"\x1b[<0;4;2M", at + Duration::from_millis(120))
                .action,
            Some(Action::Select(Select::Word(spot)))
        );
        assert_eq!(
            f.filter_at(b"\x1b[<0;4;2M", at + Duration::from_millis(240))
                .action,
            Some(Action::Select(Select::Line(spot)))
        );
        // A click a second later is a click again, and so is one next door.
        assert_eq!(
            f.filter_at(b"\x1b[<0;4;2M", at + Duration::from_secs(1))
                .action,
            Some(Action::Select(Select::From(spot)))
        );
        assert_eq!(
            f.filter_at(b"\x1b[<0;9;2M", at + Duration::from_millis(1100))
                .action,
            Some(Action::Select(Select::From(Spot { col: 9, row: 2 })))
        );
    }

    /// Nothing about the mouse is read while the terminal still has it. Every
    /// report then is the session's own doing, and reading one would be reading
    /// input meant for whatever asked for it.
    #[test]
    fn the_mouse_says_nothing_here_while_the_session_has_it() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        let dragged = f.filter(b"\x1b[<0;10;5M");
        assert_eq!(dragged.action, None);
        assert_eq!(dragged.forward, b"\x1b[<0;10;5M", "and it goes through");
    }

    /// The wheel is the client's wherever there is a history of our own for a
    /// notch to move, and for the whole attach rather than only once the view
    /// is up. The client's screen is the terminal's alternate one, which has no
    /// scrollback to offer and no alternate scroll either, so a notch that is
    /// not reported reaches nobody at all.
    #[cfg(feature = "desktop")]
    #[test]
    fn the_wheel_is_the_clients_wherever_there_is_a_history_to_move() {
        assert!(wheel_is_ours(true, false));
    }

    /// Inline, and on a host too old to answer for a window, there is nothing
    /// here for a notch to move: the terminal keeps the wheel, and with it the
    /// bare drag that selects.
    #[cfg(feature = "desktop")]
    #[test]
    fn the_terminal_keeps_the_wheel_where_there_is_no_history_of_ours() {
        assert!(!wheel_is_ours(false, false));
        assert!(!wheel_is_ours(false, true));
    }

    /// A program that asked for the mouse keeps it, history or no history.
    /// Taking it would leave two readers on one wheel, and a full-screen program
    /// draws its own scrolling from exactly these reports.
    #[cfg(feature = "desktop")]
    #[test]
    fn the_wheel_is_never_taken_from_a_session_that_asked_for_it() {
        assert!(!wheel_is_ours(true, true));
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

    /// The view's keys are read the same way the popup's are, so a held arrow
    /// keeps moving it and a released one moves nothing. A long build is
    /// scrolled by holding a key more often than by pressing one.
    #[test]
    fn an_arrow_held_down_keeps_moving_the_view() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        for keys in [&b"\x1b[A"[..], &b"\x1b[1;1:2A"[..], &b"\x1b[1;5A"[..]] {
            assert_eq!(
                f.filter(keys),
                asked(Action::Scroll(Scroll::Up(1)), Mode::Scroll),
                "{keys:?}"
            );
        }
        assert_eq!(f.filter(b"\x1b[1;1:3A").action, None, "a key let go of");
        assert_eq!(f.filter(b"\x1b[1;1:3A").mode, Mode::Scroll);
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
                rest: Vec::new(),
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

        // Rubbing out the last of nothing does nothing: a line rubbed out to
        // be typed again is a line still being typed.
        assert_eq!(
            f.filter(&[0x7f]),
            asked(Action::Find(Find::Typed), Mode::Scroll)
        );
        assert_eq!(f.needle().as_deref(), Some(""));

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

        // And a rub with nothing left to rub out keeps the prompt, so a name
        // rubbed out to be typed again can be typed again.
        f.filter(&[KEY, b'r']);
        f.filter(b"bench");
        f.filter(&[0x15]);
        assert_eq!(
            f.filter(&[0x7f]),
            asked(Action::Rename(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_name().as_deref(), Some(""));
        f.filter(b"nightly");
        assert_eq!(f.wanted_name().as_deref(), Some("nightly"));
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

        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[110u"),
            asked(Action::New, Mode::Focus)
        );

        // Shift is the alternate the terminal reports beside the key, since the
        // client's own keys read their case: `H` is not `h`, and the two go
        // opposite ways.
        let mut f = KeyFilter::new(KEY);
        assert_eq!(
            f.filter(b"\x1b[93;5u\x1b[104u"),
            asked(Action::Pick(Pick::NextGroup), Mode::Control)
        );
        assert_eq!(
            f.filter(b"\x1b[104:72;2u"),
            asked(Action::Pick(Pick::PreviousGroup), Mode::Control)
        );
        // And where no alternate was asked for, the case is worked out here.
        assert_eq!(
            f.filter(b"\x1b[104;2u"),
            asked(Action::Pick(Pick::PreviousGroup), Mode::Control)
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
                rest: Vec::new(),
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

    /// The same rule one mode along: a report is not a keystroke, so control
    /// mode outlives one. A session running something that asked for mouse
    /// tracking or focus reporting has the terminal sending these whenever the
    /// hand or the window moves, and read a byte at a time the Esc in front of
    /// one dropped the mode and typed the rest of the report into the session.
    #[test]
    fn a_report_arriving_in_control_mode_leaves_the_mode_as_it_was() {
        let mut f = KeyFilter::new(KEY);
        assert_eq!(f.filter(&[KEY]), held());
        // A drag, and the window losing the focus and getting it back.
        assert_eq!(f.filter(b"\x1b[<35;12;7M\x1b[O\x1b[I"), held());
        // Still control mode, so the next key is still the client's.
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
    }

    /// And Shift-Tab is spelt the same way and is not one of them: it is a key
    /// somebody pressed, and it walks the list backwards.
    #[test]
    fn shift_tab_is_not_taken_for_a_report() {
        let mut f = KeyFilter::new(KEY);
        f.filter(&[KEY]);
        assert_eq!(
            f.filter(SHIFT_TAB),
            asked(Action::Pick(Pick::Up), Mode::Control)
        );
    }

    /// The view is held the same way. A report reaching it while the session
    /// has the mouse read as the Esc that leaves, so a hand moving over the
    /// terminal closed the window you were reading.
    #[test]
    fn a_report_arriving_in_the_view_leaves_it_open() {
        let mut f = KeyFilter::new(KEY);
        f.set_scroll(true);
        f.filter(&[KEY, SCROLL_KEY]);
        assert_eq!(
            f.filter(b"\x1b[<35;12;7M"),
            Keystrokes {
                forward: vec![],
                action: None,
                mode: Mode::Scroll,
                rest: Vec::new(),
            }
        );
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
    /// that only arrived as `CSI 27 u` would leave a prompt with no way out of
    /// it at all.
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
                rest: Vec::new(),
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

    /// And the release of the key control mode acted on is the client's too,
    /// which is the half the rule above was missing. `Ctrl-] n` starts a
    /// session and hands the keyboard straight to it, so under a program that
    /// asked for event types the `n` coming back up was typed into the shell
    /// that had just started, and `0;1:3u` landed on somebody's prompt.
    #[test]
    fn the_release_of_the_key_that_acted_does_not_reach_the_session() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u"), held());
        assert_eq!(f.filter(b"\x1b[110;1u"), asked(Action::New, Mode::Focus));
        // Letting go of `n`, once the key has already put us back in focus.
        assert_eq!(f.filter(b"\x1b[110;1:3u"), forwarded(b""));
    }

    /// And it stays the client's for as long as the hand is on it. A key held
    /// past the repeat threshold reports every repeat, so a slot given up on
    /// the first one leaks all the rest: the same stray sequence, once a
    /// keypress, down the new session's first line.
    #[test]
    fn a_key_held_after_it_acted_leaks_none_of_its_repeats() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u"), held());
        assert_eq!(f.filter(b"\x1b[110;1u"), asked(Action::New, Mode::Focus));
        assert_eq!(f.filter(b"\x1b[110;1:2u"), forwarded(b""));
        assert_eq!(f.filter(b"\x1b[110;1:2u"), forwarded(b""));
        assert_eq!(f.filter(b"\x1b[110;1:3u"), forwarded(b""));
        // Let go of, it is the session's again like any other key.
        assert_eq!(f.filter(b"\x1b[110;1u"), forwarded(b"\x1b[110;1u"));
        assert_eq!(f.filter(b"\x1b[110;1:3u"), forwarded(b"\x1b[110;1:3u"));
    }

    /// Every key that leaves control mode has the same release to answer for,
    /// not just the one that starts a session: Enter commits the popup and
    /// lands you in whatever was highlighted.
    #[test]
    fn the_release_of_the_key_that_commits_the_popup_stays_here_too() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u"), held());
        assert_eq!(
            f.filter(b"\x1b[13;1u"),
            asked(Action::Pick(Pick::Go), Mode::Focus)
        );
        assert_eq!(f.filter(b"\x1b[13;1:3u"), forwarded(b""));
    }

    /// Holding a key is how a list of twenty sessions gets walked. Under a
    /// program that asked for event types the terminal stops repeating the
    /// plain byte and reports repeats instead, so dropping them meant holding
    /// tab moved the highlight once and then stopped, and only while such a
    /// program was running: the same hand on the same key worked everywhere
    /// else.
    #[test]
    fn a_held_key_repeats_in_the_popup() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(b"\x1b[93;5u"), held());
        // Tab pressed, then the same tab repeating twice as it is held.
        for spelling in [&b"\x1b[9;1u"[..], b"\x1b[9;1:2u", b"\x1b[9;1:2u"] {
            assert_eq!(
                f.filter(spelling),
                asked(Action::Pick(Pick::Down), Mode::Control),
                "{}",
                String::from_utf8_lossy(spelling)
            );
        }
        // And letting go still says nothing, which is the half that has to
        // keep working: the ctrl underneath it reports its own release.
        assert_eq!(f.filter(b"\x1b[9;1:3u"), held());
    }

    /// The same key held at a prompt types its character again, the way it
    /// would in any other text field.
    #[test]
    fn a_held_key_repeats_at_a_prompt() {
        let mut f = KeyFilter::default();
        f.set_scroll(true);
        f.filter(&[DEFAULT_PREFIX, b'/']);
        f.filter(b"\x1b[97;1u");
        f.filter(b"\x1b[97;1:2u");
        assert_eq!(f.needle().as_deref(), Some("aa"));
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
            asked(Action::Pick(Pick::Up), Mode::Control)
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
                rest: Vec::new(),
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
                rest: Vec::new(),
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
                rest: Vec::new(),
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
                rest: Vec::new(),
            }
        );
    }

    #[test]
    fn control_mode_stays_on_so_tab_walks_the_list() {
        // The point of the mode: one key, then as many moves as you like. What
        // moves now is the highlight in the popup rather than the client, so
        // walking three sessions is one reattach on the Enter instead of three.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Pick(Pick::Down), Mode::Control)
        );
        assert_eq!(
            f.filter(b"\t"),
            asked(Action::Pick(Pick::Down), Mode::Control)
        );
        assert_eq!(
            f.filter(SHIFT_TAB),
            asked(Action::Pick(Pick::Up), Mode::Control)
        );
        // `l` is not a move through the list but a jump out of it, so it still
        // commits: the session you came from may not even be on screen.
        assert_eq!(
            f.filter(b"l"),
            asked(Action::Switch(Motion::Last), Mode::Control)
        );
    }

    /// One gesture walks the sessions, which is what leaves the letters for
    /// the things a session is done to rather than moved through. `p` was the
    /// other half of a pair whose first half is now `new`, so it is nobody's
    /// key and the session gets it.
    #[test]
    fn the_letters_no_longer_walk_the_list() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'p']), forwarded(&[KEY, b'p']));
    }

    /// The one control key that leaves the mode. A hop puts you where another
    /// session already is and you may well want the next one; a new session is
    /// a fresh shell, and what comes after it is typing.
    #[test]
    fn the_new_key_asks_for_a_session_and_leaves_the_keyboard_in_focus() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'n']), asked(Action::New, Mode::Focus));

        // Both cases, like every key here but the two that move machine.
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'N']), asked(Action::New, Mode::Focus));
    }

    #[test]
    fn the_host_keys_move_machine_and_leave_control_mode_on() {
        // The one binding that reads its own case, so both spellings are keys
        // rather than `H` dropping back to focus as an unbound one would.
        //
        // A bigger step of the same gesture now that the machines are drawn as
        // headings in the popup: they move the highlight to the next machine's
        // first session and commit nothing, which Enter is for.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'h']),
            asked(Action::Pick(Pick::NextGroup), Mode::Control)
        );
        assert_eq!(
            f.filter(b"H"),
            asked(Action::Pick(Pick::PreviousGroup), Mode::Control)
        );
        // And walking the machine you land on carries on from there.
        assert_eq!(
            f.filter(b"\t"),
            asked(Action::Pick(Pick::Down), Mode::Control)
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
            asked(Action::Pick(Pick::Down), Mode::Control)
        );
        assert_eq!(f.filter(&[KEY]), forwarded(b""));
        assert_eq!(f.filter(b"ls"), forwarded(b"ls"));
    }

    #[test]
    fn a_literal_mode_key_still_takes_two_of_them_while_walking_the_sessions() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\t']),
            asked(Action::Pick(Pick::Down), Mode::Control)
        );
        // The first closes the popup, the second starts a fresh mode key, and
        // the third is the one that goes through.
        assert_eq!(f.filter(&[KEY, KEY, KEY]), forwarded(&[KEY]));
        assert_eq!(f.filter(b"x"), forwarded(b"x"));
    }

    #[test]
    fn shift_tab_goes_back_and_a_bare_escape_closes_the_popup() {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, 0x1b, b'[', b'Z']),
            asked(Action::Pick(Pick::Up), Mode::Control)
        );
        // An Esc with nothing behind it in the same read is a real Esc: back to
        // focus, which is also what takes the popup off the screen, and typing
        // carries on into the session.
        assert_eq!(f.filter(b"\x1b"), forwarded(b""));
        assert_eq!(f.filter(b"ls"), forwarded(b"ls"));
    }

    #[test]
    fn enter_takes_the_highlighted_row_without_reaching_the_session() {
        // Swallowed rather than forwarded: choosing a row must not also submit
        // whatever is sitting at the prompt behind the popup.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'\r']),
            asked(Action::Pick(Pick::Go), Mode::Focus)
        );
        assert_eq!(f.filter(b"x"), forwarded(b"x"));
    }

    #[test]
    fn a_mistyped_mode_key_returns_to_focus_and_keeps_your_keystrokes() {
        // Both bytes through, so the line is visibly wrong rather than
        // silently eaten while the mode sat there unnoticed.
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'v', b'i', b'm']),
            forwarded(&[KEY, b'v', b'i', b'm'])
        );
    }

    /// A filter already showing the group list, which is where `m` and `g`
    /// leave it.
    fn picking() -> KeyFilter {
        let mut f = KeyFilter::default();
        assert_eq!(
            f.filter(&[KEY, b'm']),
            asked(Action::Pick(Pick::Move), Mode::Picking)
        );
        f
    }

    /// The same gesture as in the session list. Bound in only one of them, the
    /// group list was a list you could open and not move in, so every Enter
    /// took the first row.
    #[test]
    fn the_group_list_moves_with_the_same_keys_the_session_list_does() {
        for key in [&b"\t"[..], &b"j"[..]] {
            let mut f = picking();
            assert_eq!(
                f.filter(key),
                asked(Action::Pick(Pick::Down), Mode::Picking),
                "{key:?}"
            );
        }
        for key in [SHIFT_TAB, &b"k"[..], &b"\x1b[A"[..]] {
            let mut f = picking();
            assert_eq!(
                f.filter(key),
                asked(Action::Pick(Pick::Up), Mode::Picking),
                "{key:?}"
            );
        }
    }

    /// A terminal asked to report event types stops resending the plain
    /// spelling of a held key, so an arrow held down in the popup arrives once
    /// and then only as repeats. Dropping those made holding one move the
    /// highlight exactly once, and only while a program like `pi` had the
    /// terminal in that mode.
    #[test]
    fn an_arrow_held_down_keeps_moving_a_list() {
        // Press, repeat, repeat, in the spelling those modes use.
        let held: &[&[u8]] = &[b"\x1b[A", b"\x1b[1;1:2A", b"\x1b[1;1:2A"];
        for keys in held {
            let mut f = picking();
            assert_eq!(
                f.filter(keys),
                asked(Action::Pick(Pick::Up), Mode::Picking),
                "{keys:?}"
            );
        }
        for keys in held {
            let mut f = KeyFilter::new(KEY);
            f.filter(&[KEY]);
            assert_eq!(
                f.filter(keys),
                asked(Action::Pick(Pick::Up), Mode::Control),
                "{keys:?}"
            );
        }
    }

    /// The same key with a modifier held, and the same key spelt the long way
    /// with no modifier at all: both are somebody reaching for up.
    #[test]
    fn an_arrow_spelt_the_long_way_moves_a_list() {
        for keys in [&b"\x1b[1;5B"[..], &b"\x1b[1;1:1B"[..], &b"\x1bOB"[..]] {
            let mut f = picking();
            assert_eq!(
                f.filter(keys),
                asked(Action::Pick(Pick::Down), Mode::Picking),
                "{keys:?}"
            );
        }
    }

    /// Letting go of an arrow is not a keystroke, and it must not be read as
    /// the Esc it starts with either: that would close the list under the hand
    /// still holding the key.
    #[test]
    fn letting_go_of_an_arrow_moves_nothing_and_closes_nothing() {
        let mut f = picking();
        assert_eq!(
            f.filter(b"\x1b[1;1:3A"),
            Keystrokes {
                forward: Vec::new(),
                action: None,
                mode: Mode::Picking,
                rest: Vec::new(),
            }
        );
    }

    /// Shift-Tab spelt the way a terminal in an extended-keys mode spells it,
    /// which is tab with shift rather than `CSI Z`. Read as the byte behind it
    /// alone, it walked the list the wrong way.
    #[test]
    fn shift_tab_walks_the_group_list_back_in_either_spelling() {
        for keys in [SHIFT_TAB, &b"\x1b[9;2u"[..]] {
            let mut f = picking();
            assert_eq!(
                f.filter(keys),
                asked(Action::Pick(Pick::Up), Mode::Picking),
                "{keys:?}"
            );
        }
    }

    /// Shift-Tab is spelt like a report and pressed like a key, and read a byte
    /// at a time the Esc it starts with is the Esc that closes the list.
    #[test]
    fn shift_tab_does_not_close_the_group_list() {
        let mut f = picking();
        assert_ne!(
            f.filter(SHIFT_TAB),
            asked(Action::Pick(Pick::Cancel), Mode::Focus)
        );
    }

    /// A group list is opened over the session list, so the way out of one is
    /// back to the other: both lists' hints say `esc` and the popup's own
    /// comment says it comes back to where the gesture started. It went to
    /// `Mode::Focus` instead, which closes the box, and the session list cost
    /// another `Ctrl-]` and a walk back to the row you were on.
    #[test]
    fn esc_out_of_a_group_list_goes_back_to_the_session_list() {
        for key in [&b"\x1b"[..], &b"q"[..]] {
            assert_eq!(
                picking().filter(key),
                asked(Action::Pick(Pick::Cancel), Mode::Control),
                "{key:?}"
            );
        }
        // And out of the session list it is out of the popup, as it always was.
        let mut f = KeyFilter::new(DEFAULT_PREFIX);
        f.set_mode(Mode::Control);
        assert_eq!(f.filter(b"\x1b").mode, Mode::Focus);
    }

    /// The keys that act on a session mean nothing while a group is being
    /// chosen: `m` here would be moving a move, and `d` would detach out of a
    /// gesture halfway through.
    #[test]
    fn the_session_keys_are_unbound_while_a_group_is_being_chosen() {
        for key in [&b"m"[..], &b"d"[..], &b"["[..], &b"h"[..], &b"l"[..]] {
            let mut f = picking();
            let out = f.filter(key);
            assert_eq!(out.action, None, "{key:?}");
            assert!(out.forward.is_empty(), "nothing reaches the session");
            assert_eq!(out.mode, Mode::Picking);
        }
    }

    /// Naming a new group is the same editing as the rename and the search, so
    /// the same prompt answers for it, and it comes back to the list it was
    /// opened from rather than to the session.
    #[test]
    fn naming_a_new_group_types_at_the_shared_prompt() {
        let mut f = picking();
        assert_eq!(
            f.filter(b"n"),
            asked(Action::GroupName(Rename::Open), Mode::Rename)
        );
        assert_eq!(
            f.filter(b"pi"),
            asked(Action::GroupName(Rename::Typed), Mode::Rename)
        );
        assert_eq!(f.wanted_group().as_deref(), Some("pi"));
        assert_eq!(
            f.filter(b"\r"),
            asked(Action::GroupName(Rename::Run), Mode::Picking)
        );
    }

    /// Two writes a moment apart arrive as one read, so `tab` then `Enter` is
    /// a single chunk. Every other action ends the chunk it was found in; a
    /// popup move must not, or the key that commits is thrown away.
    #[test]
    fn a_move_and_the_key_that_commits_it_survive_arriving_together() {
        let mut f = KeyFilter::default();
        let out = f.filter(&[KEY, b'\t', b'\r']);
        assert_eq!(out.action, Some(Action::Pick(Pick::Down)));
        assert_eq!(out.rest, b"\r".to_vec(), "the commit is handed back");
        let out = f.filter(&out.rest);
        assert_eq!(out.action, Some(Action::Pick(Pick::Go)));
    }

    /// And nothing else hands anything back: nobody types through a detach.
    #[test]
    fn every_other_action_still_ends_the_chunk_it_was_found_in() {
        let mut f = KeyFilter::default();
        let out = f.filter(&[KEY, b'd', b'x', b'y']);
        assert_eq!(out.action, Some(Action::Detach));
        assert!(out.rest.is_empty());
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
                rest: Vec::new(),
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
                rest: Vec::new(),
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
                rest: Vec::new(),
            }
        );
        assert_eq!(f.filter(b"d"), asked(Action::Detach, Mode::Focus));
    }

    /// The digits, which are the whole of going back: `1` is the session you
    /// are in and `2` the one you came from, whatever the box happens to be
    /// showing.
    #[test]
    fn a_digit_in_control_mode_goes_to_the_session_wearing_it() {
        for (byte, number) in [(b'1', 1), (b'5', 5), (b'9', 9)] {
            let mut f = KeyFilter::default();
            assert_eq!(f.filter(&[KEY]), held());
            assert_eq!(
                f.filter(&[byte]),
                asked(Action::Pick(Pick::Number(number)), Mode::Focus),
                "{byte:?}"
            );
        }
    }

    /// There is no zeroth session, so the key is the session's like any other
    /// unbound one: control mode drops back to focus and the keystrokes go
    /// through, mode key and all.
    #[test]
    fn zero_is_not_a_session_and_goes_to_the_session() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY, b'0']), forwarded(&[KEY, b'0']));
    }

    /// The group list has no digits: its rows are groups, and a key that acted
    /// on whatever session was highlighted behind it would act on one nobody
    /// can see. Unbound there, the same as every other session key.
    #[test]
    fn a_digit_does_nothing_in_the_group_list() {
        let mut f = picking();
        assert_eq!(
            f.filter(b"2"),
            Keystrokes {
                forward: Vec::new(),
                action: None,
                mode: Mode::Picking,
                rest: Vec::new(),
            }
        );
    }

    /// A digit is spelt the long way too while a program has the terminal in
    /// an extended-keys mode, and a key that only worked in one spelling would
    /// be a key that stops working while `pi` is running.
    #[test]
    fn a_digit_arrives_the_long_way_round_as_well() {
        let mut f = KeyFilter::default();
        assert_eq!(f.filter(&[KEY]), held());
        assert_eq!(
            f.filter(b"\x1b[50;1u"),
            asked(Action::Pick(Pick::Number(2)), Mode::Focus)
        );
    }
}
