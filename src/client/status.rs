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
use crate::style;

/// Rows kept for the mark, and so subtracted from the size the session is told.
const RESERVED: u16 = 1;

/// Blank columns between the mark and the right edge, the same gutter `mm ls`
/// puts between its columns.
const GUTTER: u16 = 2;

/// What the keys do, shown while control mode is on. Without it the mode is a
/// terminal that has stopped taking what you type for no visible reason.
const HINT: &str = "tab next  p prev  l last  d detach  esc focus";

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
}

impl Status {
    pub fn new(target: &str) -> Self {
        Self {
            target: target.to_string(),
            mode: Mode::default(),
        }
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
        // Amber in control mode, so a mode that takes your keystrokes for
        // itself is never on without you seeing it.
        let mode = match self.mode {
            Mode::Focus => style::faint(&padded(self.mode)),
            Mode::Control => style::amber(&padded(self.mode)),
        };
        let dot = style::green("●");
        let name = style::faint(&self.target);
        format!(
            "\x1b[{row};1H\x1b[2K{hint}\x1b[{row};{column}H{mode} {dot} {name}",
            row = size.rows,
            hint = self.hint(size, width),
        )
    }

    /// The key hints, at the left end of the same row, while there is room for
    /// them beside the mark. The mark wins when there is not.
    fn hint(&self, size: Size, mark: u16) -> String {
        // A blank column between the two, so they never run together.
        if self.mode != Mode::Control || columns(HINT) + 1 + mark + GUTTER > size.cols {
            return String::new();
        }
        style::faint(HINT)
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
/// Two jobs, both needing the same parse. Titles get the prefix, so the tab says
/// `mm` however often the remote shell renames it. And sequences that would take
/// the mark or its scrolling region with them are noted, so the client can put
/// both back.
///
/// Everything else passes through byte for byte. This is not a terminal
/// emulator and must never become one: it tracks just enough state to know a
/// title sequence from anything else, and to know when it is between sequences.
#[derive(Default)]
pub struct Filter {
    state: State,
    /// Bytes of a sequence that may yet turn out to be a title, held back until
    /// it is clear whether they need rewriting.
    held: Vec<u8>,
    dirty: bool,
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
                        self.note_csi(b);
                        self.release(&mut out);
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

    /// Whether the parser is between sequences, and so whether bytes of our own
    /// can be written without landing in the middle of one.
    pub fn at_boundary(&self) -> bool {
        self.state == State::Ground
    }

    /// Sequences that clear the screen or hand the margins back.
    ///
    /// A region the session sets for itself is left alone: it thinks the screen
    /// is a row shorter than it is, so anything it asks for fits inside ours.
    /// The exception is a bare `CSI r`, which means the whole screen and would
    /// let the next scroll eat the mark.
    fn note_csi(&mut self, final_byte: u8) {
        // `held` is `\x1b[` then the parameters then the final byte.
        let params = &self.held[2..self.held.len() - 1];
        match final_byte {
            b'J' => self.dirty = true,
            b'r' if params.is_empty() => self.dirty = true,
            // Soft reset, which includes the margins.
            b'p' if params == b"!" => self.dirty = true,
            // A full-screen program switching screens: the alternate screen has
            // margins of its own, and coming back restores none of ours.
            b'h' | b'l' => {
                let Ok(text) = std::str::from_utf8(params) else {
                    return;
                };
                let Some(modes) = text.strip_prefix('?') else {
                    return;
                };
                if modes
                    .split(';')
                    .filter_map(|mode| mode.parse::<u16>().ok())
                    .any(|mode| matches!(mode, 47 | 1047 | 1049))
                {
                    self.dirty = true;
                }
            }
            _ => {}
        }
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
