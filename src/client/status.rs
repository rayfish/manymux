//! The marks that say you are inside a session rather than at a plain shell.
//!
//! Two of them. A dim `focus ● host/name` in the bottom-right corner, on a row
//! the session is never told about, and `mm ` in front of whatever the session
//! puts in the window title. Both are drawn by the client and neither is visible to
//! the session: the node keeps the title the program set, so `mm ls` still shows
//! it unprefixed.
//!
//! Everything here is string building and parsing, kept out of
//! [`super::attach`] so it can be tested without a terminal.

use crate::client::attach::Mode;
use crate::proto::Size;
use crate::settings::Screen;
use crate::style;

/// Rows kept for the mark, and so subtracted from the size the session is told.
const RESERVED: u16 = 1;

/// Blank columns between the mark and the right edge, the same gutter `mm ls`
/// puts between its columns.
const GUTTER: u16 = 2;

/// What the keys do, shown while control mode is on. Without it the mode is a
/// terminal that has stopped taking what you type for no visible reason.
const HINT: &str = "tab next  p prev  h host  l last  d detach  esc focus";

/// The same, for the view over the session's history, where the keys are
/// different again and the screen is showing something the session did not
/// print just now.
const SCROLL_HINT: &str = "pgup/pgdn page  g/G ends  esc live";

/// Columns kept for the mode's name, which is the width of the longer of the
/// two. Fixed, so the mark does not jump sideways when the mode changes.
const MODE: usize = "control".len();

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

/// Sequences that paint the mark and hold the row it lives on.
pub struct Status {
    target: String,
    /// The mode the row says the keyboard is in.
    mode: Mode,
    /// Something the client has to say for itself, which takes the hints' place
    /// while it is up. A paste is the only thing that has anything to say so
    /// far, and it says it here because a full-screen program owns every other
    /// row on the screen.
    notice: Option<String>,
    /// How far back the view over the session's history is, while it is up.
    /// A place of its own rather than a notice, because a notice goes away
    /// after a few seconds and this is true for as long as the view is.
    scrolled: Option<u64>,
}

impl Status {
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            mode: Mode::default(),
            notice: None,
            scrolled: None,
        }
    }

    /// Say how far back the view is, or that it has gone. The caller repaints.
    pub fn set_scrolled(&mut self, lines: Option<u64>) {
        self.scrolled = lines;
    }

    /// Put a message on the row. The caller repaints, and takes it off again
    /// once it has been up long enough.
    pub fn set_notice(&mut self, notice: &str) {
        self.notice = Some(notice.to_string());
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
        let width = columns(&format!("{:>MODE$} ● {}", self.mode.name(), self.target));
        // Right-aligned, and dropped rather than wrapped on a terminal too
        // narrow to hold it and its gutter: a wrapped mark would scroll the
        // screen.
        if width + GUTTER > size.cols {
            return String::new();
        }
        let column = size.cols - width + 1 - GUTTER;
        // Amber in control mode and in the view, so a mode that takes your
        // keystrokes for itself, or a screen that is not showing the session as
        // it stands, is never on without you seeing it.
        let mode = match self.mode {
            Mode::Focus => style::faint(&padded(self.mode)),
            Mode::Control | Mode::Scroll => style::amber(&padded(self.mode)),
        };
        let dot = style::green("●");
        let name = style::faint(&self.target);
        format!(
            "\x1b[{row};1H\x1b[2K{hint}\x1b[{row};{column}H{mode} {dot} {name}",
            row = size.rows,
            hint = self.hint(size, width),
        )
    }

    /// What sits at the left end of the same row: whatever the client has to
    /// say, else the key hints while control mode is on, while there is room
    /// for it beside the mark. The mark wins when there is not.
    fn hint(&self, size: Size, mark: u16) -> String {
        let scrolled = self
            .scrolled
            .map(|lines| format!("{lines} back  {SCROLL_HINT}"));
        let (text, styled) = match (&self.notice, &scrolled, self.mode) {
            // A notice outranks the hints: it is the answer to a key that was
            // just pressed, and the hints will be back in a few seconds.
            (Some(notice), _, _) => (notice.as_str(), style::amber as fn(&str) -> String),
            // Where the view is, which is the one thing the screen itself
            // cannot say: it is showing lines that look exactly like the ones
            // the session printed a moment ago.
            (None, Some(scrolled), _) => (scrolled.as_str(), style::amber as fn(&str) -> String),
            (None, None, Mode::Control) => (HINT, style::faint as fn(&str) -> String),
            (None, None, Mode::Focus | Mode::Scroll) => return String::new(),
        };
        // A blank column between the two, so they never run together.
        if columns(text) + 1 + mark + GUTTER > size.cols {
            return String::new();
        }
        styled(text)
    }

    /// `mm ` in front of a title, or in front of the target when the session has
    /// not set one.
    fn title(&self, text: &str) -> String {
        format!("\x1b]0;{}\x07", prefixed(text))
    }
}

/// How many columns a piece of unstyled text takes.
fn columns(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

/// The mode's name in a fixed width, so the mark keeps its column when the
/// shorter of the two names is showing.
fn padded(mode: Mode) -> String {
    format!("{:>MODE$}", mode.name())
}

/// The scrolling region the session lives in: everything above the mark.
fn region(size: Size) -> String {
    format!("\x1b[1;{}r", size.rows - RESERVED)
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

/// Rewrites the session's output on its way to the terminal.
///
/// Three jobs, all needing the same parse. Titles get the prefix, so the tab
/// says `mm` however often the remote shell renames it. Sequences that would
/// take the mark or its scrolling region with them are noted, so the client can
/// put both back. And the session's own switches between the primary and
/// alternate screens are swallowed, because that screen is the client's.
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
                    _ => out.push(b),
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
            assert_eq!(through(&mut filter, &format!("a{switch}b")), "ab");
            assert!(filter.take_switched(), "{switch}");
            assert!(filter.take_dirty(), "{switch}");
            assert!(filter.at_boundary());
        }
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
        // `  focus ● srv/zsh` is 17 columns, so with two to spare on the right
        // it starts at column 62 of row 24.
        assert!(painted.contains("\x1b[24;62H"), "{painted:?}");
        assert!(painted.contains("focus"), "{painted:?}");
    }

    #[test]
    fn the_mode_keeps_its_width_so_the_mark_does_not_jump() {
        let focus = Status::new("srv/zsh").repaint(Size::new(80, 24));
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let control = status.repaint(Size::new(80, 24));

        assert!(control.contains("control"), "{control:?}");
        assert!(
            focus.contains("\x1b[24;62H") && control.contains("\x1b[24;62H"),
            "the mark moved when the mode changed"
        );
    }

    #[test]
    fn the_hints_only_show_in_control_mode() {
        let mut status = Status::new("srv/zsh");
        assert!(!status.repaint(Size::new(80, 24)).contains("tab next"));
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(80, 24));
        assert!(painted.contains("tab next"), "{painted:?}");
        // And the mark keeps its place beside them.
        assert!(painted.contains("\x1b[24;62H"), "{painted:?}");
    }

    #[test]
    fn a_row_too_narrow_for_both_keeps_the_mark() {
        let mut status = Status::new("srv/zsh");
        status.set_mode(Mode::Control);
        let painted = status.repaint(Size::new(40, 24));
        assert!(!painted.contains("tab next"), "{painted:?}");
        assert!(painted.contains("\x1b[24;22H"), "{painted:?}");
    }
}
