//! The marks that say you are inside a session rather than at a plain shell.
//!
//! Two of them. A dim `● host/name` in the bottom-right corner, on a row
//! the session is never told about, and `mm ` in front of whatever the session
//! puts in the window title. Both are drawn by the client and neither is visible to
//! the session: the node keeps the title the program set, so `mm ls` still shows
//! it unprefixed.
//!
//! Everything here is string building and parsing, kept out of
//! [`super::attach`] so it can be tested without a terminal.

use std::time::Duration;

use crate::client::attach::Mode;
use crate::client::scroll::Selected;
use crate::proto::Size;
use crate::settings::Screen;
use crate::style;

/// Rows kept for the mark, and so subtracted from the size the session is told.
const RESERVED: u16 = 1;

/// Blank columns between the mark and the right edge, the same gutter `mm ls`
/// puts between its columns.
const GUTTER: u16 = 2;

/// What the keys do, shown while control mode is on and there is no popup to
/// say it: a window too small to hold a box, which is the only time this row is
/// all there is. Without it the mode is a terminal that has stopped taking what
/// you type for no visible reason. The highlight goes in front of them there,
/// which is [`Popped::Cramped`].
///
/// Cut from the end when the row cannot hold all of it, rather than dropped
/// whole: the mark takes what it takes, a long target name leaves little
/// beside it, and there are more keys than an eighty-column row has room for.
/// So they are in the order a hand reaches for them, and the last ones are the
/// ones a session can be driven without.
const HINTS: &[&str] = &[
    "tab next",
    "s-tab prev",
    "⏎ go",
    // Early, because it is the only way into the view: the wheel is left to
    // the terminal so that a drag selects, and a key nobody can find is a
    // feature nobody has.
    "[ scroll",
    "n new",
    "r rename",
    "d detach",
    "esc focus",
    "h host",
    "l last",
];

/// What the group lists answer to, for the same row when there is no box to
/// carry their own hints.
///
/// All or nothing rather than a ladder, being two keys: `choose` because the
/// highlight sitting beside it already says which list is open, and there is no
/// room to say `move` and `show` separately for the sake of a word.
const PICK_HINTS: &str = "⏎ choose  esc back";

/// What sits between one hint and the next, the same gutter again.
const HINT_GAP: &str = "  ";

/// The same, for the view over the session's history, where the keys are
/// different again and the screen is showing something the session did not
/// print just now.
const SCROLL_HINT: &str = "pgup/pgdn page  g/G ends  esc live";

/// Below this the mark costs more than it is worth, and on a two-row terminal it
/// would leave the session a single line. Give the whole screen to the session
/// instead and draw nothing.
const MIN_ROWS: u16 = 6;

/// Whether a terminal this size has room to spare for the mark.
fn marked(size: Size) -> bool {
    size.rows >= MIN_ROWS
}

/// The size to hand the session: one row short, so nothing it draws can land on
/// the marked row and nothing it scrolls can push through it.
pub fn session_size(size: Size) -> Size {
    if marked(size) {
        Size::new(size.cols, size.rows - RESERVED)
    } else {
        size
    }
}

/// What the popup is doing to this row.
///
/// Three states rather than two, because a popup that could not be drawn is not
/// the same as no popup: the mode is on, the highlight is moving under every
/// press, and this row is the only surface left to say so.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub enum Popped {
    /// None up, so the row says what the keys do itself.
    #[default]
    None,
    /// One up and drawing its own hints inside the box, so this row says
    /// nothing about keys: two hint lines at once is one too many.
    Drawn,
    /// One up with no room to draw. The row stands in for it, and carries which
    /// list is open and which row the highlight is on.
    Cramped(String),
}

/// Sequences that paint the mark and hold the row it lives on.
pub struct Status {
    target: String,
    /// The mode the row says the keyboard is in.
    mode: Mode,
    /// What the popup has left this row to do.
    popped: Popped,
    /// The group the run is narrowed to, if any. Drawn in front of the target,
    /// because the row already answers "which session" and this answers "which
    /// work", and a narrowing you cannot see is a set of switch keys that
    /// quietly stopped reaching half your sessions.
    group: Option<String>,
    /// Something the client has to say for itself, which takes the hints' place
    /// while it is up. A paste is the only thing that has anything to say so
    /// far, and it says it here because a full-screen program owns every other
    /// row on the screen.
    notice: Option<String>,
    /// How far back the view over the session's history is, while it is up.
    /// A place of its own rather than a notice, because a notice goes away
    /// after a few seconds and this is true for as long as the view is.
    scrolled: Option<u64>,
    /// How much is under the highlight, while a selection is being drawn.
    ///
    /// The one thing about a selection the screen cannot say for itself: a
    /// drag held at an edge walks the view under the highlight at up to a
    /// dozen lines a move, so the end it started from went past the top of the
    /// window a second ago and there is nowhere else to count it.
    selected: Option<Selected>,
    /// What is being typed at a prompt, while one is open: the mark that says
    /// which prompt it is, and the text so far. Outranks everything else on the
    /// row: it is the only thing there that is being changed a keystroke at a
    /// time, and a row that does not show what you are typing is a row you
    /// cannot type into.
    prompt: Option<(&'static str, String)>,
    /// What the last search found, once it has been run.
    searching: Option<String>,
    /// Whether this client is only watching. True for the whole attach rather
    /// than a mode that comes and goes, which is why it is a field here and not
    /// one of [`Mode`]: nothing you press changes it.
    watching: bool,
    /// Whether there is a session at the other end of this at all. False while
    /// a lost connection is being waited out, which is the one time the screen
    /// shows a session that nothing is reaching.
    lost: bool,
}

impl Status {
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            mode: Mode::default(),
            popped: Popped::None,
            group: None,
            notice: None,
            scrolled: None,
            selected: None,
            prompt: None,
            searching: None,
            watching: false,
            lost: false,
        }
    }

    /// Say what the popup has left this row to do. The caller repaints.
    pub fn set_popup(&mut self, popped: Popped) {
        self.popped = popped;
    }

    /// Which group the run is narrowed to, or none.
    pub fn set_group(&mut self, group: Option<&str>) {
        self.group = group.map(str::to_string);
    }

    /// Say that this client cannot type into the session, for as long as it is
    /// attached.
    pub fn watching(mut self) -> Self {
        self.watching = true;
        self
    }

    /// Say that there is no session at the other end: the screen is showing one
    /// as it was last painted, and the connection to it has gone.
    pub fn lost(mut self) -> Self {
        self.lost = true;
        self
    }

    /// Show a different session name, for the one thing that changes it while
    /// an attach is up: a rename the host has agreed to. The caller repaints,
    /// and gives the window the new name with [`Self::retitle`].
    pub fn set_target(&mut self, target: &str) {
        self.target = target.to_string();
    }

    /// Name the window again, for when the target has changed under it. The
    /// same sequence [`Self::setup`] opens with, and it lasts exactly as long:
    /// until the session sets a title of its own.
    pub fn retitle(&self) -> String {
        self.title(&self.target)
    }

    /// Say how far back the view is, or that it has gone. The caller repaints.
    pub fn set_scrolled(&mut self, lines: Option<u64>) {
        self.scrolled = lines;
        if lines.is_none() {
            // The view has closed, and what a search found went with it, along
            // with the selection it was drawn on.
            self.searching = None;
            self.prompt = None;
            self.selected = None;
        }
    }

    /// Say how much is under the highlight, or that nothing is. The caller
    /// repaints.
    pub fn set_selected(&mut self, selected: Option<Selected>) {
        self.selected = selected;
    }

    /// Show what is being typed at the search prompt, or take the prompt away.
    pub fn set_prompt(&mut self, needle: Option<String>) {
        self.prompt = needle.map(|text| ("/", text));
    }

    /// The same for the prompt that renames the session, told apart by being
    /// spelled out: `/` is what a search is called everywhere, and a rename is
    /// called nothing in particular.
    pub fn set_renaming(&mut self, name: Option<String>) {
        self.prompt = name.map(|text| ("rename ", text));
    }

    /// And for the prompt that names a group to put the highlighted session
    /// in, which is the same editing again and a different question.
    pub fn set_grouping(&mut self, name: Option<String>) {
        self.prompt = name.map(|text| ("group ", text));
    }

    /// Show what the last search found: the needle, and which match of how
    /// many the view is on.
    pub fn set_searching(&mut self, searching: Option<String>) {
        self.searching = searching;
    }

    /// Put a message on the row. The caller repaints, and takes it off again
    /// once it has been up long enough.
    pub fn set_notice(&mut self, notice: &str) {
        self.notice = Some(notice.to_string());
    }

    /// Whether the row is already saying something for the client. Asked by
    /// the pump loop, which owns the clock every notice is taken down by and
    /// does not otherwise know one was put there before it started.
    pub fn has_notice(&self) -> bool {
        self.notice.is_some()
    }

    pub fn clear_notice(&mut self) {
        self.notice = None;
    }

    /// Say which mode the row is to show. The caller repaints afterwards.
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
    }

    /// What the row currently says, so a caller can tell when it has stopped
    /// matching the key filter and needs redrawing.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the client is showing something of its own, rather than the
    /// session as it stands.
    ///
    /// Which is what says where the terminal's own cursor belongs. That cursor
    /// is the session's and sits wherever the session left it: under a box or a
    /// view it blinks on a row that has nothing to do with what is being shown,
    /// and at a prompt it says the typing is going somewhere it is not. So the
    /// caller takes it away for as long as this is true.
    ///
    /// A window too small for a box ([`Popped::Cramped`]) is not this: the
    /// session's screen is whole there, on every row but the mark, and the
    /// cursor on it is still the session's own.
    pub fn showing_its_own(&self) -> bool {
        matches!(self.popped, Popped::Drawn) || self.scrolled.is_some() || self.prompt.is_some()
    }

    /// Sent once, after the alternate screen is up: name the window, fence the
    /// session into the rows above the mark, and draw it.
    pub fn setup(&self, size: Size) -> String {
        format!("{}{}", self.title(&self.target), self.repaint(size))
    }

    /// Re-fence the scrolling region and redraw, after something that may have
    /// taken either away: a clear, a reset, a full-screen program coming or
    /// going, or the terminal changing size.
    ///
    /// Wrapped in a cursor save/restore because setting the scrolling region
    /// homes the cursor, and the session's idea of where it is has to survive
    /// the repaint.
    pub fn repaint(&self, size: Size) -> String {
        if !marked(size) {
            // No room for it now (a shrunken window). Give the region back, or
            // the session would keep scrolling inside a fence it no longer
            // knows the shape of.
            return "\x1b7\x1b[r\x1b8".to_string();
        }
        format!("\x1b7{}{}\x1b8", region(size), self.mark(size))
    }

    /// The mark itself, drawn a gutter in from the right end of the reserved
    /// row, with the key hints beside it while control mode is on.
    fn mark(&self, size: Size) -> String {
        // The group drops first when the row runs out of room: which session
        // you are in is the thing this row exists to say, and the narrowing is
        // worth knowing but not worth losing the target over.
        let mut prefix = match &self.group {
            Some(group) if !group.is_empty() => format!("{group} · "),
            _ => String::new(),
        };
        let mut width = columns(&format!("● {prefix}{}", self.target));
        if width + GUTTER > size.cols {
            prefix.clear();
            width = columns(&format!("● {}", self.target));
        }
        // Right-aligned, and dropped rather than wrapped on a terminal too
        // narrow to hold it and its gutter: a wrapped mark would scroll the
        // screen.
        if width + GUTTER > size.cols {
            return String::new();
        }
        let column = size.cols - width + 1 - GUTTER;
        // The dot is the mode, and the row does not name one. Filled and green
        // while the session has the keyboard; hollow and amber the moment the
        // client takes it, so a mode that eats your keystrokes, or a screen not
        // showing the session as it stands, is never on without you seeing it.
        // Which mode is beside it already: control puts the hints there, the
        // view how far back it is, a rename what is being typed.
        // Watching outranks the mode, because it is the one thing on this row
        // that no keystroke changes: the keyboard reaches nothing whatever mode
        // it is in, and a green dot promising otherwise would be a lie for the
        // whole attach rather than for a moment.
        // A lost connection outranks both, for the same reason and more of it:
        // there is no session at the other end to have the keyboard, and the
        // screen above the row is showing one that cannot say so itself.
        let dot = match (self.lost, self.watching, self.mode) {
            (true, _, _) => style::amber("○"),
            (false, true, _) => style::faint("◦"),
            (false, false, Mode::Focus) => style::green("●"),
            (false, false, Mode::Control | Mode::Picking | Mode::Scroll | Mode::Rename) => {
                style::amber("○")
            }
        };
        let name = style::faint(&format!("{prefix}{}", self.target));
        format!(
            "\x1b[{row};1H\x1b[2K{hint}\x1b[{row};{column}H{dot} {name}",
            row = size.rows,
            hint = self.hint(size, width),
        )
    }

    /// What sits at the left end of the same row: whatever the client has to
    /// say, else the key hints while control mode is on, in as much room as the
    /// mark leaves beside it. The mark wins when there is none: everything here
    /// is either dropped or, in the hints' case, cut short to fit.
    fn hint(&self, size: Size, mark: u16) -> String {
        // A prompt being typed into outranks the lot: it is the only thing on
        // this row that changes a keystroke at a time.
        if let Some((label, prompt)) = &self.prompt {
            let typed = format!("{label}{prompt}");
            return if columns(&typed) + 1 + mark + GUTTER > size.cols {
                String::new()
            } else {
                // The label is the client talking, and stays the colour every
                // other mode marker on this row is. What follows it is text
                // being typed, and reads as text rather than as more chrome.
                format!("{}{}", style::amber(label), style::value(prompt))
            };
        }
        let scrolled = self
            .scrolled
            .map(|lines| match (&self.selected, &self.searching) {
                // A selection outranks what a search found, being the thing moving
                // under the hand: the search was run before the drag started and
                // has not changed since. Both keep the way out on the row and drop
                // the rest of the hints, there being room for one of the three.
                (Some(selected), _) => format!("{lines} back  {selected} selected  esc live"),
                // What a search found says where you are better than a line count
                // does, and both will not fit.
                (None, Some(searching)) => format!("{searching}  n next  esc live"),
                (None, None) => format!("{lines} back  {SCROLL_HINT}"),
            });
        let (text, styled) = match (&self.notice, &scrolled, self.mode) {
            // A notice outranks the hints: it is the answer to a key that was
            // just pressed, and the hints will be back in a few seconds.
            (Some(notice), _, _) => (notice.as_str(), style::amber as fn(&str) -> String),
            // Where the view is, which is the one thing the screen itself
            // cannot say: it is showing lines that look exactly like the ones
            // the session printed a moment ago.
            (None, Some(scrolled), _) => (scrolled.as_str(), style::amber as fn(&str) -> String),
            // The popup's job, on a window with no room for a popup. Control
            // mode draws its own hints inside the box when there is one, and
            // two hint lines at once is one too many, so this arm is reached
            // only where the box could not be drawn at all.
            (None, None, mode @ (Mode::Control | Mode::Picking))
                if self.popped != Popped::Drawn =>
            {
                let room = size.cols.saturating_sub(1 + mark + GUTTER);
                let text = self.without_a_box(mode, room);
                return if text.is_empty() {
                    String::new()
                } else {
                    style::faint(&text)
                };
            }
            // Rename is here for the sake of the match: that mode is never on
            // without a prompt, which was answered above.
            (
                None,
                None,
                Mode::Control | Mode::Picking | Mode::Focus | Mode::Scroll | Mode::Rename,
            ) => return String::new(),
        };
        // A blank column between the two, so they never run together.
        if columns(text) + 1 + mark + GUTTER > size.cols {
            return String::new();
        }
        styled(text)
    }

    /// The row standing in for a popup there was no room to draw: the highlight
    /// first, then as many keys as fit beside it.
    ///
    /// The highlight leads because it is the thing moving under the hand. A tab
    /// that walks a list nobody can see is a key that has stopped working as far
    /// as anyone watching can tell, and on a window this small there is no other
    /// surface to say otherwise on.
    fn without_a_box(&self, mode: Mode, room: u16) -> String {
        let Popped::Cramped(showing) = &self.popped else {
            return hints(room);
        };
        // The keys go before the highlight does: a list you cannot see at all
        // is worse than one whose keys you have to remember.
        if columns(showing) > room {
            return String::new();
        }
        let left = room.saturating_sub(columns(showing) + columns(HINT_GAP));
        let keys = match mode {
            Mode::Picking if columns(PICK_HINTS) <= left => PICK_HINTS.to_string(),
            Mode::Picking => String::new(),
            // The session list's keys are this row's own ladder, cut from the
            // end the way it always cuts them.
            _ => hints(left),
        };
        if keys.is_empty() {
            return showing.clone();
        }
        format!("{showing}{HINT_GAP}{keys}")
    }

    /// `mm ` in front of a title, or in front of the target when the session has
    /// not set one.
    fn title(&self, text: &str) -> String {
        format!("\x1b]0;{}\x07", prefixed(text))
    }
}

/// As many of the hints as fit in `room` columns, in their own order and with
/// the same gutter between them as the rest of the row uses.
///
/// The first one that does not fit ends it, rather than being stepped over for
/// a shorter one behind it: the order is what says which keys matter, and a
/// list that reorders itself as the window is dragged is a list nobody can
/// learn.
fn hints(room: u16) -> String {
    let mut text = String::new();
    for hint in HINTS {
        let next = if text.is_empty() {
            (*hint).to_string()
        } else {
            format!("{text}{HINT_GAP}{hint}")
        };
        if columns(&next) > room {
            break;
        }
        text = next;
    }
    text
}

/// How many columns a piece of unstyled text takes.
fn columns(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

/// What the row says while a lost connection is being waited out.
///
/// A screen somebody walks back up to has to answer three things before they
/// touch anything: how long it has been sitting there, whether anything is
/// still being tried, and how to stop. Which session it is about is not among
/// them, being on the mark two columns to the right.
///
/// `retry_in` is how long until the next attempt, or `None` while one is out
/// there. Saying which is the difference between a client that is working and
/// one that has stopped, and from the screen alone they look identical: the
/// session is painted exactly as it was either way.
pub fn waiting_notice(lost_for: Duration, retry_in: Option<Duration>) -> String {
    let trying = match retry_in {
        // Never zero: a row that counts down to nothing and sits there reads as
        // a clock that has stopped rather than one about to tick over.
        Some(left) => format!("retrying in {}s", ceiling(left).max(1)),
        None => "reconnecting".to_string(),
    };
    format!(
        "lost {} ago{HINT_GAP}{trying}{HINT_GAP}ctrl-c to stop",
        roughly(lost_for)
    )
}

/// Seconds, rounded up, so a countdown starts at the delay it was given rather
/// than a second short of it.
fn ceiling(left: Duration) -> u64 {
    left.as_secs() + u64::from(left.subsec_nanos() > 0)
}

/// How long ago something was, in the one unit somebody reads it in.
///
/// Coarser as it grows, because that is how the answer is used: seconds tell
/// you to wait, minutes tell you to go and do something else, and nobody who
/// has been away for an hour cares which second of it this is.
fn roughly(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    format!("{}h {}m", minutes / 60, minutes % 60)
}

/// The scrolling region the session lives in: everything above the mark.
fn region(size: Size) -> String {
    format!("\x1b[1;{}r", size.rows - RESERVED)
}

/// Whether a byte on solid ground puts something on the screen.
///
/// The control bytes that get here move the cursor or ring the bell, and a
/// dump's line breaks are among them: an erase owed to the screen must not be
/// spent on one of those, or it lands before the first glyph of the screen it
/// was meant to make room for rather than after the last of the one before.
fn paints(b: u8) -> bool {
    b >= 0x20 && b != 0x7f
}

/// The prefix, applied once. A title the session repeats every prompt must not
/// grow an `mm mm mm ` in front of it.
fn prefixed(text: &str) -> String {
    if text.starts_with("mm ") {
        text.to_string()
    } else {
        format!("mm {text}")
    }
}

/// The erase a swallowed screen switch owes the terminal.
///
/// Switching buffers is how a terminal is told the screen is about to be a
/// different one, and it clears the one being switched to. Swallowing the
/// switch leaves that undone, so it is done here instead, in the one place that
/// knows the switch happened.
///
/// The pen goes back to default because the erase clears to the current
/// background, and a program that left one set would otherwise paint the whole
/// screen its colour. The cursor is saved and put back because this lands
/// wherever the session had it, and what comes next is written there.
const SWITCHED: &str = concat!(
    "\x1b7",   // save the cursor, and the pen with it
    "\x1b[0m", // default attributes, so the erase clears to the usual background
    "\x1b[2J", // the screen the other buffer had
    "\x1b8",   // the pen and the cursor back
);

/// Rewrites the session's output on its way to the terminal.
///
/// Three jobs, all needing the same parse. Titles get the prefix, so the tab
/// says `mm` however often the remote shell renames it. Sequences that would
/// take the mark or its scrolling region with them are noted, so the client can
/// put both back. And the session's own switches between the primary and
/// alternate screens are swallowed, because that screen is the client's, and
/// [`SWITCHED`] is written in their place before whatever paints next.
///
/// Everything else passes through byte for byte. This is not a terminal
/// emulator and must never become one: it tracks just enough state to know a
/// title sequence from anything else, and to know when it is between sequences.
pub struct Filter {
    /// Whether the client's screen mode owns the screen, and so whether the
    /// session's own switches between the two are its to make.
    owns_the_screen: bool,
    state: State,
    /// Bytes of a sequence that may yet turn out to be a title, held back until
    /// it is clear whether they need rewriting.
    held: Vec<u8>,
    dirty: bool,
    switched: bool,
    /// Whether the session has the terminal on its alternate screen. Only
    /// inline, where the switches reach the terminal, is this the terminal's
    /// state as well as the session's.
    alternate: bool,
    /// Whether the program in the session has asked to be told about the
    /// mouse. While it has, the wheel is its business and the client neither
    /// turns tracking on nor reads a report.
    mouse: bool,
    /// Whether a swallowed switch has left the screen owing an erase, taken by
    /// the next byte that paints.
    ///
    /// Toggled rather than set, because two switches with nothing painted
    /// between them are a round trip back to the buffer already on the screen
    /// and owe nothing. `avt` emits exactly that: a dump of a session on the
    /// primary screen that has used the alternate one names both switches and
    /// paints between neither.
    owed: bool,
}

impl Default for Filter {
    fn default() -> Self {
        Self::new(Screen::default())
    }
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    /// DCS, SOS, PM and APC: opaque payloads that run until a string
    /// terminator. Nothing in one is rewritten, and nothing may be written into
    /// one, which is what keeps a kitty image or a tmux passthrough intact.
    ///
    /// `bel_ends` is for the payload of an OSC too long to hold, which ends the
    /// way an OSC does.
    Str {
        bel_ends: bool,
    },
    StrEscape {
        bel_ends: bool,
    },
}

/// The longest title kept. A sequence longer than this is passed through
/// unrewritten rather than buffered without limit.
const MAX_OSC: usize = 4096;

impl Filter {
    pub fn new(screen: Screen) -> Self {
        Self {
            owns_the_screen: screen.mode().owns_the_screen(),
            state: State::default(),
            held: Vec::new(),
            dirty: false,
            switched: false,
            alternate: false,
            mouse: false,
            owed: false,
        }
    }

    /// Whether the session has asked for mouse reports. The client's own
    /// tracking is the complement of this: two programs reading one wheel is
    /// one of them reading keystrokes meant for the other.
    pub fn session_mouse(&self) -> bool {
        self.mouse
    }

    /// Whether the session is sitting in a full-screen program's alternate
    /// screen. Inline that is the terminal's screen too, so the teardown has to
    /// pop it or a detach from inside vim hands back vim's screen.
    pub fn on_alternate(&self) -> bool {
        self.alternate
    }

    /// Feed a chunk of session output, and get back what to write to the
    /// terminal.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len() + 16);
        for &b in chunk {
            match self.state {
                State::Ground => match b {
                    0x1b => {
                        self.state = State::Escape;
                        self.held.push(b);
                    }
                    _ => {
                        // The first thing to paint since a switch was
                        // swallowed, and so the last moment the screen the
                        // other buffer had can be erased without taking this
                        // byte with it.
                        if self.owed && paints(b) {
                            out.extend_from_slice(SWITCHED.as_bytes());
                            self.owed = false;
                        }
                        out.push(b);
                    }
                },
                State::Escape => {
                    self.held.push(b);
                    match b {
                        b'[' => self.state = State::Csi,
                        b']' => self.state = State::Osc,
                        // RIS resets everything, margins included.
                        b'c' => {
                            self.dirty = true;
                            self.release(&mut out);
                        }
                        b'P' | b'X' | b'^' | b'_' => {
                            self.flush(&mut out);
                            self.state = State::Str { bel_ends: false };
                        }
                        // Another escape: the first one was abandoned.
                        0x1b => {}
                        _ => self.release(&mut out),
                    }
                }
                State::Csi => {
                    self.held.push(b);
                    if (0x40..=0x7e).contains(&b) {
                        if self.note_csi(b) {
                            self.release(&mut out);
                        } else {
                            self.held.clear();
                            self.state = State::Ground;
                        }
                    }
                }
                State::Osc => match b {
                    0x07 => self.finish_osc(&mut out, "\x07"),
                    0x1b => self.state = State::OscEscape,
                    _ => {
                        self.held.push(b);
                        // Too long to be a title worth rewriting, and a clipboard
                        // write is the usual reason. Let the bytes go, and treat
                        // what is left as the opaque string it is rather than
                        // pretending we are back on solid ground.
                        if self.held.len() > MAX_OSC {
                            self.flush(&mut out);
                            self.state = State::Str { bel_ends: true };
                        }
                    }
                },
                State::OscEscape => match b {
                    b'\\' => self.finish_osc(&mut out, "\x1b\\"),
                    // Not a terminator after all: the escape was part of the
                    // payload.
                    _ => {
                        self.held.push(0x1b);
                        self.held.push(b);
                        self.state = State::Osc;
                    }
                },
                State::Str { bel_ends } => {
                    out.push(b);
                    match b {
                        0x07 if bel_ends => self.state = State::Ground,
                        0x1b => self.state = State::StrEscape { bel_ends },
                        _ => {}
                    }
                }
                State::StrEscape { bel_ends } => {
                    out.push(b);
                    match b {
                        b'\\' => self.state = State::Ground,
                        0x1b => {}
                        _ => self.state = State::Str { bel_ends },
                    }
                }
            }
        }
        out
    }

    /// Whether the mark and its scrolling region may have been undone since this
    /// was last asked. Clears the flag.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::take(&mut self.dirty)
    }

    /// Whether a screen switch was swallowed since this was last asked. Clears
    /// the flag.
    ///
    /// The caller owes the terminal a screen after one: the switch the terminal
    /// would have restored from never reached it, so the only place the right
    /// picture still exists is the node's model of the session.
    pub fn take_switched(&mut self) -> bool {
        std::mem::take(&mut self.switched)
    }

    /// Whether the parser is between sequences, and so whether bytes of our own
    /// can be written without landing in the middle of one.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground
    }

    /// Sequences that clear the screen or hand the margins back, and the ones
    /// that are not the session's to send at all. Answers whether the sequence
    /// may go on to the terminal.
    ///
    /// A region the session sets for itself is left alone: it thinks the screen
    /// is a row shorter than it is, so anything it asks for fits inside ours.
    /// The exception is a bare `CSI r`, which means the whole screen and would
    /// let the next scroll eat the mark.
    fn note_csi(&mut self, final_byte: u8) -> bool {
        // `held` is `\x1b[` then the parameters then the final byte.
        let params = &self.held[2..self.held.len() - 1];
        match final_byte {
            b'J' => self.dirty = true,
            b'r' if params.is_empty() => self.dirty = true,
            // Soft reset, which includes the margins.
            b'p' if params == b"!" => self.dirty = true,
            b'h' | b'l' => return self.note_modes(final_byte),
            _ => {}
        }
        true
    }

    /// A full-screen program switching screens.
    ///
    /// Inline the switch is the session's to make and the terminal's to
    /// remember, so it goes through and is only noted. On a screen the client
    /// owns it cannot: the client is already there and draws the mark there, so
    /// letting `?1049l` through would pop the terminal back to the shell's
    /// screen while the client is still attached, and the session and the mark
    /// would end up painted over the scrollback of the terminal you started
    /// from. The node makes the same choice on the way in:
    /// `events::REPLAYED_MODES` leaves these out, so attaching to a session
    /// sitting in a full-screen program draws its screen without switching
    /// yours.
    ///
    /// Other modes in the same sequence are none of our business and go on
    /// without the ones taken out.
    fn note_modes(&mut self, final_byte: u8) -> bool {
        let params = &self.held[2..self.held.len() - 1];
        let Ok(text) = std::str::from_utf8(params) else {
            return true;
        };
        let Some(modes) = text.strip_prefix('?') else {
            return true;
        };
        // Any of the tracking modes: which one the program picked decides what
        // it is told about, not whether it is told anything.
        if modes
            .split(';')
            .any(|mode| matches!(mode.parse::<u16>(), Ok(9 | 1000 | 1002 | 1003)))
        {
            self.mouse = final_byte == b'h';
        }
        let ours = |mode: &&str| matches!(mode.parse::<u16>(), Ok(47 | 1047 | 1049));
        let kept: Vec<&str> = modes.split(';').filter(|m| !ours(m)).collect();
        if kept.len() == modes.split(';').count() {
            return true;
        }
        // The screen underneath is about to be a different one, and the mark
        // and its region do not survive the trip either way.
        self.dirty = true;
        self.alternate = final_byte == b'h';
        if !self.owns_the_screen {
            return true;
        }
        self.switched = true;
        // The screen the terminal would have cleared for itself, owed until
        // there is something to clear it for.
        self.owed = !self.owed;
        if kept.is_empty() {
            return false;
        }
        self.held = format!("\x1b[?{}{}", kept.join(";"), final_byte as char).into_bytes();
        true
    }

    /// A finished OSC: rewrite it if it is a title, pass it on if it is not.
    ///
    /// 0 sets icon name and title, 1 the icon name, 2 the title. All three get
    /// the prefix, because a shell that sets one after the other would otherwise
    /// leave the tab named by whichever came last.
    fn finish_osc(&mut self, out: &mut Vec<u8>, terminator: &str) {
        self.state = State::Ground;
        // `held` is `\x1b]` then the payload.
        let payload = String::from_utf8_lossy(&self.held[2..]).into_owned();
        let title = payload
            .split_once(';')
            .filter(|(ps, _)| matches!(*ps, "0" | "1" | "2"));
        match title {
            Some((ps, text)) => {
                out.extend_from_slice(format!("\x1b]{ps};{}", prefixed(text)).as_bytes());
            }
            None => out.extend_from_slice(&self.held),
        }
        out.extend_from_slice(terminator.as_bytes());
        self.held.clear();
    }

    /// Let the held bytes go, leaving the state alone.
    fn flush(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.held);
        self.held.clear();
    }

    /// Let them go, and call the sequence finished.
    fn release(&mut self, out: &mut Vec<u8>) {
        self.flush(out);
        self.state = State::Ground;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn through(filter: &mut Filter, input: &str) -> String {
        String::from_utf8(filter.feed(input.as_bytes())).unwrap()
    }

    /// The terminal's own cursor is the session's for as long as the session is
    /// what is on the screen. A row too small for a box is still that: the
    /// screen under it is the session's, whole.
    #[test]
    fn the_cursor_is_the_sessions_until_the_client_shows_something_of_its_own() {
        let mut status = Status::new("host/name");
        assert!(!status.showing_its_own(), "nothing is up");
        status.set_popup(Popped::Cramped("sessions: name".to_string()));
        assert!(!status.showing_its_own(), "a row is not a screen");
        status.set_popup(Popped::Drawn);
        assert!(status.showing_its_own(), "a box is");
        status.set_popup(Popped::None);
        status.set_scrolled(Some(40));
        assert!(status.showing_its_own(), "and so is the view");
        status.set_scrolled(None);
        status.set_renaming(Some("na".to_string()));
        assert!(status.showing_its_own(), "and a prompt being typed into");
        status.set_renaming(None);
        assert!(
            !status.showing_its_own(),
            "and then it is the session's again"
        );
    }

    /// The row is the only thing on the screen that changes while a connection
    /// is being waited out: everything above it is the session as it was
    /// painted before the drop. So it has to say that something is still
    /// happening, and when the next thing happens.
    #[test]
    fn the_row_says_a_connection_is_being_waited_for_and_for_how_long() {
        let waiting = waiting_notice(Duration::from_secs(4), Some(Duration::from_secs(8)));
        assert!(waiting.contains("lost 4s ago"), "{waiting}");
        assert!(waiting.contains("retrying in 8s"), "{waiting}");
        assert!(waiting.contains("ctrl-c to stop"), "{waiting}");
    }

    /// An attempt is out there and the screen cannot show it: a client trying
    /// to reach a machine and one that has given up paint the same nothing.
    #[test]
    fn the_row_says_when_an_attempt_is_out_rather_than_counting_down_to_one() {
        let trying = waiting_notice(Duration::from_secs(90), None);
        assert!(trying.contains("reconnecting"), "{trying}");
        assert!(!trying.contains("retrying in"), "{trying}");
    }

    /// The countdown is what says the client is still working, so it must not
    /// spend the last second of every delay reading zero.
    #[test]
    fn a_countdown_rounds_up_and_never_reaches_zero() {
        let mid = waiting_notice(Duration::ZERO, Some(Duration::from_millis(7_100)));
        assert!(mid.contains("retrying in 8s"), "{mid}");
        let last = waiting_notice(Duration::ZERO, Some(Duration::from_millis(20)));
        assert!(last.contains("retrying in 1s"), "{last}");
        let over = waiting_notice(Duration::ZERO, Some(Duration::ZERO));
        assert!(over.contains("retrying in 1s"), "{over}");
    }

    /// Coming back to a screen that has been sitting there since lunch, the
    /// question is how long, not how many seconds.
    #[test]
    fn how_long_it_has_been_lost_is_said_in_the_unit_it_is_read_in() {
        assert_eq!(roughly(Duration::from_secs(0)), "0s");
        assert_eq!(roughly(Duration::from_secs(59)), "59s");
        assert_eq!(roughly(Duration::from_secs(60)), "1m");
        assert_eq!(roughly(Duration::from_secs(59 * 60 + 59)), "59m");
        assert_eq!(roughly(Duration::from_secs(3 * 3600 + 5 * 60)), "3h 5m");
    }

    /// The dot says who has the keyboard, and while the connection is gone
    /// nobody does: the session it would go to is on a machine this client
    /// cannot reach, and the screen above the row is showing that session
    /// exactly as it was left.
    #[test]
    fn the_dot_does_not_promise_a_session_that_is_not_there() {
        let live = Status::new("gpu-box/build").repaint(Size::new(80, 24));
        let lost = Status::new("gpu-box/build")
            .lost()
            .repaint(Size::new(80, 24));
        assert!(live.contains('●'), "{live:?}");
        assert!(!lost.contains('●'), "{lost:?}");
        assert!(lost.contains('○'), "{lost:?}");
    }

    /// It shares the row with the mark, and the mark wins: a notice too long
    /// for the terminal is not shown at all, which would leave the wait with
    /// nothing to say for itself on the width people actually use.
    #[test]
    fn the_whole_notice_fits_beside_the_mark_on_an_eighty_column_row() {
        let mut status = Status::new("gpu-box/nightly-build");
        status.set_notice(&waiting_notice(
            Duration::from_secs(3 * 3600 + 5 * 60),
            Some(Duration::from_secs(10)),
        ));
        let row = status.repaint(Size::new(80, 24));
        assert!(row.contains("retrying in 10s"), "{row:?}");
    }

    /// A drag held at an edge walks the view under the highlight, so the end
    /// it started from is off the top of the window and the row is the only
    /// place left that can say how much is under it. It takes the scroll
    /// hints' place and keeps the way out, there being room for one of the
    /// three.
    #[test]
    fn a_selection_takes_the_scroll_hints_place_and_keeps_the_way_out() {
        let mut status = Status::new("gpu-box/build");
        status.set_scrolled(Some(140));
        let hints = status.repaint(Size::new(80, 24));
        assert!(hints.contains(SCROLL_HINT), "{hints:?}");

        status.set_selected(Some(Selected::Lines(37)));
        let row = status.repaint(Size::new(80, 24));
        assert!(row.contains("140 back"), "{row:?}");
        assert!(row.contains("37 lines selected"), "{row:?}");
        assert!(row.contains("esc live"), "{row:?}");
        assert!(!row.contains("pgup/pgdn"), "{row:?}");
    }

    /// And it outranks what a search found, being the thing moving under the
    /// hand: the search was run before the drag started and has not changed
    /// since.
    #[test]
    fn a_selection_outranks_what_a_search_found() {
        let mut status = Status::new("gpu-box/build");
        status.set_scrolled(Some(140));
        status.set_searching(Some("/needle  2/9".into()));
        status.set_selected(Some(Selected::Chars(12)));
        let row = status.repaint(Size::new(80, 24));
        assert!(row.contains("12 chars selected"), "{row:?}");
        assert!(!row.contains("needle"), "{row:?}");
    }

    /// The view closing takes the selection with it, the same way it takes
    /// what a search found: there is no highlight left to count.
    #[test]
    fn the_view_closing_takes_the_selection_off_the_row() {
        let mut status = Status::new("gpu-box/build");
        status.set_scrolled(Some(140));
        status.set_selected(Some(Selected::Lines(37)));
        status.set_scrolled(None);
        status.set_scrolled(Some(140));
        let row = status.repaint(Size::new(80, 24));
        assert!(!row.contains("selected"), "{row:?}");
    }

    #[test]
    fn the_session_is_told_the_screen_is_a_row_shorter() {
        assert_eq!(session_size(Size::new(80, 24)), Size::new(80, 23));
    }

    #[test]
    fn a_terminal_with_no_room_keeps_every_row() {
        let tiny = Size::new(80, 3);
        assert_eq!(session_size(tiny), tiny);
        // And the region goes back, rather than fencing off a row of three.
        assert!(!Status::new("srv/zsh").repaint(tiny).contains("\x1b[1;"));
    }

    #[test]
    fn plain_output_is_passed_through_untouched() {
        let mut filter = Filter::default();
        let text = "hello \x1b[31mworld\x1b[0m\r\n";
        assert_eq!(through(&mut filter, text), text);
        assert!(!filter.take_dirty());
    }

    #[test]
    fn titles_are_prefixed() {
        let mut filter = Filter::default();
        assert_eq!(
            through(&mut filter, "\x1b]2;~/src\x07"),
            "\x1b]2;mm ~/src\x07"
        );
        // The tab name too, which is the one oh-my-zsh sets last.
        assert_eq!(
            through(&mut filter, "\x1b]1;~/src\x1b\\"),
            "\x1b]1;mm ~/src\x1b\\"
        );
    }

    #[test]
    fn a_prefix_is_not_added_twice() {
        let mut filter = Filter::default();
        assert_eq!(
            through(&mut filter, "\x1b]0;mm srv/zsh\x07"),
            "\x1b]0;mm srv/zsh\x07"
        );
    }

    #[test]
    fn other_osc_sequences_are_left_alone() {
        let mut filter = Filter::default();
        let notify = "\x1b]777;notify;done;built\x07";
        assert_eq!(through(&mut filter, notify), notify);
        let colour = "\x1b]11;#000000\x07";
        assert_eq!(through(&mut filter, colour), colour);
    }

    #[test]
    fn a_title_split_across_chunks_is_still_rewritten() {
        let mut filter = Filter::default();
        assert_eq!(through(&mut filter, "\x1b]2;~/sr"), "");
        assert_eq!(through(&mut filter, "c\x07"), "\x1b]2;mm ~/src\x07");
    }

    #[test]
    fn a_clear_means_the_mark_needs_drawing_again() {
        let mut filter = Filter::default();
        through(&mut filter, "\x1b[2J");
        assert!(filter.take_dirty());
        assert!(!filter.take_dirty(), "the flag is taken, not left behind");
    }

    #[test]
    fn giving_the_margins_back_means_the_region_needs_setting_again() {
        let mut filter = Filter::default();
        through(&mut filter, "\x1b[r");
        assert!(filter.take_dirty());

        // A region the session sets for itself fits inside ours, so it stands.
        through(&mut filter, "\x1b[3;20r");
        assert!(!filter.take_dirty());
    }

    #[test]
    fn a_full_screen_program_coming_and_going_means_both_need_setting_again() {
        let mut filter = Filter::default();
        through(&mut filter, "\x1b[?1049h");
        assert!(filter.take_dirty());
        through(&mut filter, "\x1b[?1049l");
        assert!(filter.take_dirty());
        through(&mut filter, "\x1bc");
        assert!(filter.take_dirty());
        // Modes that have nothing to do with the screen do not.
        through(&mut filter, "\x1b[?2004h");
        assert!(!filter.take_dirty());
    }

    /// The screen the mark is drawn on belongs to the client. A session that
    /// switched it would leave the client attached to a terminal showing the
    /// shell it was started from, with the session painted over the scrollback.
    #[test]
    fn a_session_never_gets_to_switch_the_screen_out_from_under_the_client() {
        for switch in ["\x1b[?1049h", "\x1b[?1049l", "\x1b[?1047l", "\x1b[?47h"] {
            let mut filter = Filter::default();
            // The switch itself never reaches the terminal. What takes its
            // place is the erase it would have done, and it goes before the
            // first thing painted on the screen it was owed for.
            assert_eq!(
                through(&mut filter, &format!("a{switch}b")),
                format!("a{SWITCHED}b")
            );
            assert!(filter.take_switched(), "{switch}");
            assert!(filter.take_dirty(), "{switch}");
            assert!(filter.at_boundary());
        }
    }

    /// The bug this was written for: a dump paints the primary buffer, names
    /// the switch to the alternate one, and paints that. Swallowing the switch
    /// and nothing else left both buffers on one screen, so a session sitting
    /// in a full-screen program showed the scrollback of the shell that started
    /// it wherever the program had not painted.
    #[test]
    fn a_dump_paints_the_alternate_screen_on_an_erased_one() {
        let mut filter = Filter::default();
        let dump = through(&mut filter, "shell\r\n\x1b[?1047h\x1b[1;1Hprogram");
        let erase = dump.find("\x1b[2J").expect("nothing was erased");
        assert!(
            dump[..erase].contains("shell"),
            "the screen was erased before the buffer under it was painted: {dump:?}"
        );
        assert!(
            dump[erase..].contains("program"),
            "the alternate screen paints on what the primary one left: {dump:?}"
        );
    }

    /// A dump of a session on the primary screen that has used the alternate
    /// one names both switches and paints between neither, and erasing there
    /// would throw away the screen the same dump had just painted.
    #[test]
    fn a_screen_the_session_only_passed_through_is_not_erased() {
        let mut filter = Filter::default();
        let dump = through(&mut filter, "shell\x1b[?1047h\x1b[?1047l\x1b[2;1H$ ");
        assert!(!dump.contains("\x1b[2J"), "{dump:?}");
    }

    /// The other direction, which is a full-screen program exiting while you
    /// are attached: the screen it had is not the shell's, and the shell must
    /// not come back to it.
    #[test]
    fn a_program_leaving_the_alternate_screen_erases_before_the_shell_paints() {
        let mut filter = Filter::default();
        assert_eq!(through(&mut filter, "\x1b[?1049l"), "");
        assert_eq!(through(&mut filter, "\r\n$ "), format!("\r\n{SWITCHED}$ "));
    }

    /// Inline the terminal made the switch itself and cleared or kept the
    /// screen the way it always does. An erase of ours on top of that is a
    /// screenful of the scrollback thrown away.
    #[test]
    fn a_screen_the_terminal_switches_itself_is_never_erased_by_us() {
        let mut filter = Filter::new(Screen::Inline);
        let out = through(&mut filter, "\x1b[?1049hprogram");
        assert!(!out.contains("\x1b[2J"), "{out:?}");
    }

    /// Inline the screen the session switches to is the terminal's own, so the
    /// switch is the session's to make: vim gets the terminal's alternate
    /// screen and the scrollback underneath it is left alone.
    #[test]
    fn a_session_switches_the_screen_itself_when_it_is_the_terminals() {
        let mut filter = Filter::new(Screen::Inline);
        assert_eq!(through(&mut filter, "a\x1b[?1049hb"), "a\x1b[?1049hb");
        assert!(
            !filter.take_switched(),
            "nothing was swallowed to owe a screen for"
        );
        assert!(filter.take_dirty(), "the mark does not survive the switch");
        assert!(filter.on_alternate());

        assert_eq!(through(&mut filter, "\x1b[?1049l"), "\x1b[?1049l");
        assert!(!filter.on_alternate());
    }

    /// Which is what the teardown needs: detaching from inside a full-screen
    /// program has to put the terminal back on the screen the shell is on.
    #[test]
    fn the_older_spelling_of_the_switch_counts_too() {
        let mut filter = Filter::new(Screen::Inline);
        through(&mut filter, "\x1b[?1047h");
        assert!(filter.on_alternate());
        through(&mut filter, "\x1b[?1047l");
        assert!(!filter.on_alternate());
    }

    #[test]
    fn a_swallowed_switch_keeps_the_modes_beside_it() {
        let mut filter = Filter::default();
        assert_eq!(
            through(&mut filter, "\x1b[?1049;1000;2004h"),
            "\x1b[?1000;2004h"
        );
        assert!(filter.take_switched());
    }

    #[test]
    fn every_other_mode_goes_through_untouched() {
        let mut filter = Filter::default();
        assert_eq!(
            through(&mut filter, "\x1b[?25l\x1b[?1000h"),
            "\x1b[?25l\x1b[?1000h"
        );
        assert!(!filter.take_switched());
    }

    #[test]
    fn an_image_is_not_mistaken_for_anything() {
        let mut filter = Filter::default();
        // A kitty graphics payload, carrying what would otherwise read as a
        // title sequence and a clear.
        let apc = "\x1b_Gf=100,a=T;\x1b]2;not a title\x07\x1b[2J\x1b\\";
        assert_eq!(through(&mut filter, apc), apc);
        assert!(!filter.take_dirty());
        assert!(filter.at_boundary(), "the terminator ended it");
    }

    #[test]
    fn a_passthrough_sequence_survives_whole() {
        let mut filter = Filter::default();
        // tmux's DCS passthrough doubles the escapes inside it.
        let dcs = "\x1bPtmux;\x1b\x1b]2;inner\x07\x1b\\";
        assert_eq!(through(&mut filter, dcs), dcs);
    }

    #[test]
    fn a_clipboard_write_too_long_to_hold_is_still_a_sequence() {
        let mut filter = Filter::default();
        let payload = "A".repeat(MAX_OSC + 100);
        let osc52 = format!("\x1b]52;c;{payload}\x07");
        assert_eq!(through(&mut filter, &osc52), osc52);
        assert!(
            filter.at_boundary(),
            "the bell ended it, rather than leaving us mid-sequence forever"
        );
    }

    #[test]
    fn a_half_read_sequence_is_not_a_safe_place_to_draw() {
        let mut filter = Filter::default();
        through(&mut filter, "text\x1b[1;2");
        assert!(!filter.at_boundary());
        through(&mut filter, "H");
        assert!(filter.at_boundary());
    }

    #[test]
    fn the_mark_is_dropped_rather_than_wrapped_on_a_narrow_terminal() {
        let status = Status::new("a-very-long-host/a-very-long-session");
        let painted = status.repaint(Size::new(10, 24));
        assert!(painted.contains("\x1b[1;23r"), "the region still holds");
        assert!(!painted.contains("●"));
    }

    #[test]
    fn the_mark_sits_a_gutter_in_from_the_right_end_of_the_reserved_row() {
        let painted = Status::new("srv/zsh").repaint(Size::new(80, 24));
        // `● srv/zsh` is 9 columns, so with two to spare on the right it starts
        // at column 70 of row 24.
        assert!(painted.contains("\x1b[24;70H"), "{painted:?}");
        assert!(painted.contains("● "), "{painted:?}");
    }

    /// The row names no mode: what mode it is is said by what sits beside the
    /// mark, and the dot only has to say whether the keyboard is the session's.
    #[test]
    fn the_dot_is_hollow_whenever_the_client_has_the_keyboard() {
        let focus = Status::new("srv/zsh").repaint(Size::new(80, 24));
        assert!(focus.contains("●") && !focus.contains("○"), "{focus:?}");
        assert!(!focus.contains("focus"), "{focus:?}");

        for mode in [Mode::Control, Mode::Scroll, Mode::Rename] {
            let mut status = Status::new("srv/zsh");
            status.set_mode(mode);
            let painted = status.repaint(Size::new(80, 24));
            assert!(painted.contains("○"), "{mode:?}: {painted:?}");
            // And the mark keeps its column: both dots are one glyph wide.
            assert!(painted.contains("\x1b[24;70H"), "{mode:?}: {painted:?}");
        }
    }

    #[test]
    fn the_hints_only_show_in_control_mode() {
        let mut status = Status::new("srv/zsh");
        assert!(!status.repaint(Size::new(80, 24)).contains("tab next"));
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("tab next"), "{painted:?}");
        // Including the keys that are their own gesture rather than a motion,
        // which are the ones a session can be driven without knowing.
        assert!(painted.contains("r rename"), "{painted:?}");
        assert!(painted.contains("d detach"), "{painted:?}");
        // And the mark keeps its place beside them.
        assert!(painted.contains("\x1b[24;70H"), "{painted:?}");
    }

    /// The wheel opens the view too, but the key is the way in for a hand that
    /// would rather not reach for the mouse, and the only one on a session that
    /// asked for the mouse itself. A way in nobody can see is no way in.
    #[test]
    fn the_hints_offer_the_key_that_opens_the_view() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("[ scroll"), "{painted:?}");
    }

    /// Starting a session is the one thing on the row that leaves the session
    /// you are in for one that did not exist a moment ago, so it is worth a
    /// hint of its own: `n` walked the list until this build, and a hand that
    /// learnt it there has to be told what it does now.
    #[test]
    fn the_hints_offer_the_key_that_starts_a_session() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("n new"), "{painted:?}");
        // And both directions of the one gesture that walks the list.
        assert!(painted.contains("tab next"), "{painted:?}");
        assert!(painted.contains("s-tab prev"), "{painted:?}");
    }

    /// A row with room for some of them shows some of them, in order, rather
    /// than going blank because the last one would not fit.
    #[test]
    fn a_row_too_narrow_for_all_the_hints_cuts_them_from_the_end() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(40, 24));
        assert!(painted.contains("tab next  s-tab prev"), "{painted:?}");
        assert!(!painted.contains("l last"), "{painted:?}");
        assert!(painted.contains("\x1b[24;30H"), "{painted:?}");
    }

    #[test]
    fn a_row_too_narrow_for_any_of_them_keeps_the_mark() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(19, 24));
        assert!(!painted.contains("tab next"), "{painted:?}");
        assert!(painted.contains("\x1b[24;9H"), "{painted:?}");
    }

    /// The row is the only place a name being typed shows, so it has to say
    /// which prompt is open as well as what is in it: a `/` and a word are not
    /// the same question.
    #[test]
    fn the_rename_prompt_says_so_and_shows_what_is_typed() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Rename);
        status.set_renaming(Some("nightly-bench".to_string()));
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("rename nightly-bench"), "{painted:?}");
        // And the mark keeps the column it has in every other mode, hollow
        // because what is typed here is not the session's.
        assert!(painted.contains("○"), "{painted:?}");
        assert!(painted.contains("\x1b[24;70H"), "{painted:?}");
    }

    /// A rename the host agreed to: the mark names the session it is called
    /// now, and the window is named again with it, for one that has set no
    /// title of its own and so is showing the target.
    #[test]
    fn the_mark_follows_a_rename() {
        let mut status = Status::new("srv/zsh");
        status.set_target("srv/build");
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("srv/build"), "{painted:?}");
        assert!(!painted.contains("srv/zsh"), "{painted:?}");
        assert_eq!(status.retitle(), "\x1b]0;mm srv/build\x07");
    }

    /// The bug this was written for: control mode is the popup, so a window
    /// with no room to draw one was a client that had taken the keyboard and a
    /// tab that changed nothing anybody could see. The row is the only surface
    /// left, and the highlight is the thing that moves.
    #[test]
    fn a_window_too_small_for_the_popup_says_which_row_the_highlight_is_on() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        status.set_popup(Popped::Cramped("sessions: build".to_string()));
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("sessions: build"), "{painted:?}");
        // And the keys after it, in the room the highlight leaves.
        assert!(painted.contains("tab next"), "{painted:?}");
        assert!(painted.contains("\x1b[24;70H"), "{painted:?}");
    }

    /// The box carries its own hints, and two hint lines at once is one too
    /// many. Nothing about the popup reaches this row while there is one.
    #[test]
    fn a_popup_that_drew_leaves_this_row_out_of_it() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        status.set_popup(Popped::Drawn);
        let painted = status.repaint(Size::new(80, 24));
        assert!(!painted.contains("tab next"), "{painted:?}");
        // The mark is the row's own and stays where it always is.
        assert!(painted.contains("\x1b[24;70H"), "{painted:?}");
    }

    /// The group list answers to different keys, and saying `d detach` over one
    /// would be naming a key that does something else there. Which list it is
    /// comes from the highlight beside them, so the keys can stay short enough
    /// to fit whole.
    #[test]
    fn the_group_list_gets_its_own_keys_rather_than_the_session_ladder() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Picking);
        status.set_popup(Popped::Cramped("move \"build\" to: pi".to_string()));
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("move \"build\" to: pi"), "{painted:?}");
        assert!(painted.contains("⏎ choose"), "{painted:?}");
        assert!(painted.contains("esc back"), "{painted:?}");
        assert!(!painted.contains("d detach"), "{painted:?}");
    }

    /// The keys give way before the highlight does: a list whose keys you have
    /// to remember still works, and one you cannot see at all does not.
    #[test]
    fn the_highlight_is_the_last_thing_the_row_gives_up() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        status.set_popup(Popped::Cramped("sessions: build".to_string()));

        let narrow = status.repaint(Size::new(30, 24));
        assert!(narrow.contains("sessions: build"), "{narrow:?}");
        assert!(!narrow.contains("tab next"), "{narrow:?}");

        // And where even that will not fit, the mark wins, the same as every
        // other thing that shares this row with it.
        let narrower = status.repaint(Size::new(19, 24));
        assert!(!narrower.contains("sessions"), "{narrower:?}");
        assert!(narrower.contains("\x1b[24;9H"), "{narrower:?}");
    }

    /// One prompt slot, so opening one takes the other's place rather than
    /// drawing both on top of each other.
    #[test]
    fn a_prompt_outranks_the_hints_and_the_notice_alike() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        status.set_notice("pasted 2.1 MB");
        assert!(status.repaint(Size::new(80, 24)).contains("pasted"));

        status.set_renaming(Some("api".to_string()));
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("rename api"), "{painted:?}");
        assert!(!painted.contains("pasted"), "{painted:?}");
        assert!(!painted.contains("tab next"), "{painted:?}");

        // And taking it away gives the row back to whatever was under it.
        status.set_renaming(None);
        assert!(status.repaint(Size::new(80, 24)).contains("pasted"));
    }
}
