//! What a session that asked for the mouse is told about a drag.
//!
//! The desktop settles this without knowing anything: the terminal in front of
//! somebody encodes the wheel, and `client::attach::keys::wheel_is_ours` only
//! decides whether the bytes are read here or passed through. A phone has no
//! terminal to encode for it, so a drag over a session running a full-screen
//! program reached nobody at all: the history view opened over it, and the
//! alternate screen has no history anywhere to show, `avt` giving that buffer a
//! scrollback limit of zero and the node's window therefore ending at the top
//! of the screen. From outside that is a gesture that does nothing.
//!
//! So the notches are encoded here, which needs the two things the terminal
//! would have known: whether the session asked for reports at all, and which
//! spelling it asked for them in. Both are read out of the session's own
//! output, and both arrive on an attach as well as during one, since the node
//! replays them with the screen (`node::events::REPLAYED_MODES`).
//!
//! The scanner is deliberately small. It is not a second copy of
//! `client::status::Filter`, which parses the same stream for the desktop:
//! that one rewrites what it reads, on behalf of a terminal and a mark row
//! that neither exist here, and feeding this end's output through it to learn
//! one bit would put a rewriter in front of the emulator. What is copied is a
//! list of mode numbers fixed by the spec, and nothing about how they are
//! handled.

/// Where a gesture is, in cells, counted from the top left at zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct At {
    pub col: u16,
    pub row: u16,
}

/// Whether a session has asked for mouse reports, and in which spelling.
#[derive(Default)]
pub struct Tracking {
    state: State,
    /// The private parameters of the sequence being read, `?` and all.
    csi: Vec<u8>,
    /// Any of the tracking modes: which one a program picked decides what it
    /// is told about, not whether it is told anything.
    wants: bool,
    encoding: Encoding,
}

/// How a report is spelled. Whichever the program last asked for.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    /// One byte per coordinate, offset by 32, which is why a report cannot
    /// name a column past 223.
    #[default]
    X10,
    /// `CSI < b ; col ; row M`, the one every modern program asks for.
    Sgr,
    /// `CSI b ; col ; row M`, with the button still offset by 32.
    Urxvt,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
}

/// Longest parameter run kept. A real private mode sequence is a few bytes,
/// and anything longer is not one being written a byte at a time.
const MAX_CSI: usize = 64;

/// The highest coordinate the one-byte spelling can carry.
const X10_MAX: u16 = 223;

impl Tracking {
    /// Bytes the session printed.
    ///
    /// Only `ESC [` starts a sequence here, so the payload of an OSC cannot
    /// flip a mode by containing the text of one. A DCS or a tmux passthrough
    /// carrying a real `ESC [ ? 1000 h` inside it can, and that is left alone:
    /// a program printing the bytes that switch a mode on switches it on in
    /// every terminal, and this one is not the place to start disagreeing.
    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            match self.state {
                State::Ground => {
                    if byte == 0x1b {
                        self.state = State::Escape;
                    }
                }
                State::Escape => {
                    self.csi.clear();
                    // Anything else is a sequence this has no interest in, and
                    // a second escape starts again rather than being eaten.
                    self.state = match byte {
                        b'[' => State::Csi,
                        0x1b => State::Escape,
                        _ => State::Ground,
                    };
                }
                State::Csi => match byte {
                    0x20..=0x3f if self.csi.len() < MAX_CSI => self.csi.push(byte),
                    // A run too long to be a mode sequence is read to its end
                    // and dropped, rather than ending the sequence here and
                    // reading the rest of it as text.
                    0x20..=0x3f => {}
                    0x40..=0x7e => {
                        self.note(byte);
                        self.state = State::Ground;
                    }
                    // Not a sequence at all: an escape someone printed.
                    _ => self.state = State::Ground,
                },
            }
        }
    }

    /// A finished CSI, which is only of interest as `CSI ? modes h` or `l`.
    fn note(&mut self, final_byte: u8) {
        if !matches!(final_byte, b'h' | b'l') {
            return;
        }
        let csi = std::mem::take(&mut self.csi);
        let Ok(text) = std::str::from_utf8(&csi) else {
            return;
        };
        let Some(modes) = text.strip_prefix('?') else {
            return;
        };
        let on = final_byte == b'h';
        for mode in modes.split(';').filter_map(|mode| mode.parse::<u16>().ok()) {
            match mode {
                9 | 1000 | 1002 | 1003 => self.wants = on,
                // An encoding switched off goes back to the one every terminal
                // starts in rather than to whatever was asked for before it.
                1006 | 1016 => self.encoding = if on { Encoding::Sgr } else { Encoding::X10 },
                1015 => self.encoding = if on { Encoding::Urxvt } else { Encoding::X10 },
                _ => {}
            }
        }
    }

    /// Whether the session is reading the mouse itself.
    ///
    /// While it is, a drag belongs to it: two readers on one wheel is one of
    /// them reading input meant for the other, and a full-screen program draws
    /// its own scrolling from exactly these reports.
    pub fn wanted(&self) -> bool {
        self.wants
    }

    /// One wheel notch at a cell, spelled the way the session asked for.
    ///
    /// `col` and `row` are zero based, the way the widget counts them, and go
    /// out one based, the way every spelling of a report counts them.
    pub fn wheel(&self, up: bool, col: u16, row: u16) -> Vec<u8> {
        if !self.wants {
            return Vec::new();
        }
        // 64 and 65 are the wheel's two directions. A notch has no release,
        // so there is one report and nothing to follow it.
        let button: u16 = if up { 64 } else { 65 };
        let col = col.saturating_add(1);
        let row = row.saturating_add(1);
        match self.encoding {
            Encoding::Sgr => format!("\x1b[<{button};{col};{row}M").into_bytes(),
            Encoding::Urxvt => format!("\x1b[{};{col};{row}M", button + 32).into_bytes(),
            Encoding::X10 => {
                // A cell this spelling cannot name is reported at the edge it
                // can, which is what every terminal does with one: a notch at
                // the wrong column is worth more than no notch at all.
                let col = col.min(X10_MAX) as u8;
                let row = row.min(X10_MAX) as u8;
                vec![0x1b, b'[', b'M', (button + 32) as u8, 32 + col, 32 + row]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Tracking;

    fn watching(output: &str) -> Tracking {
        let mut tracking = Tracking::default();
        tracking.feed(output.as_bytes());
        tracking
    }

    #[test]
    fn a_session_that_asked_for_nothing_is_sent_nothing() {
        let tracking = watching("hello\r\n");
        assert!(!tracking.wanted());
        assert!(tracking.wheel(true, 0, 0).is_empty());
    }

    #[test]
    fn a_program_that_asked_for_the_mouse_is_sent_the_notch() {
        let tracking = watching("\x1b[?1000h");
        assert!(tracking.wanted());
        assert_eq!(tracking.wheel(true, 9, 4), b"\x1b[M\x60\x2a\x25".to_vec());
    }

    #[test]
    fn the_sgr_spelling_is_used_where_it_was_asked_for() {
        let tracking = watching("\x1b[?1002h\x1b[?1006h");
        assert_eq!(tracking.wheel(true, 9, 4), b"\x1b[<64;10;5M".to_vec());
        assert_eq!(tracking.wheel(false, 0, 0), b"\x1b[<65;1;1M".to_vec());
    }

    #[test]
    fn one_sequence_can_switch_on_both_the_tracking_and_the_spelling() {
        let tracking = watching("\x1b[?1002;1006h");
        assert!(tracking.wanted());
        assert_eq!(tracking.wheel(true, 0, 0), b"\x1b[<64;1;1M".to_vec());
    }

    /// The one that matters for leaving a program: the drag has to come back
    /// to the history view the moment the session stops reading the mouse.
    #[test]
    fn a_program_on_its_way_out_gives_the_mouse_back() {
        let tracking = watching("\x1b[?1002;1006h\x1b[?1002l\x1b[?1006l");
        assert!(!tracking.wanted());
    }

    #[test]
    fn the_urxvt_spelling_keeps_the_button_offset() {
        let tracking = watching("\x1b[?1000h\x1b[?1015h");
        assert_eq!(tracking.wheel(true, 9, 4), b"\x1b[96;10;5M".to_vec());
    }

    /// A dump paints with a great many sequences that are not modes, and one
    /// of them ending in `h` or `l` must not be read as one.
    #[test]
    fn an_ordinary_sequence_is_not_a_mode() {
        let tracking = watching("\x1b[2K\x1b[4h\x1b[1;5H\x1b[?1000h");
        assert!(tracking.wanted());
        let plain = watching("\x1b[4h\x1b[20h");
        assert!(!plain.wanted());
    }

    /// The modes arrive on an attach as well as during one, and a chunk
    /// boundary can fall anywhere in them.
    #[test]
    fn a_sequence_split_across_two_chunks_is_still_read() {
        let mut tracking = Tracking::default();
        tracking.feed(b"\x1b[?10");
        tracking.feed(b"02;1006h");
        assert!(tracking.wanted());
        assert_eq!(tracking.wheel(false, 0, 0), b"\x1b[<65;1;1M".to_vec());
    }

    /// A title is text, and text that happens to spell a mode is not one.
    #[test]
    fn the_payload_of_an_osc_cannot_switch_a_mode_on() {
        let tracking = watching("\x1b]0;[?1000h\x07");
        assert!(!tracking.wanted());
    }

    /// The column a one-byte spelling cannot name is reported at the edge it
    /// can, rather than wrapping round to the start of the row.
    #[test]
    fn a_column_past_what_the_old_spelling_holds_is_clamped() {
        let tracking = watching("\x1b[?1000h");
        assert_eq!(tracking.wheel(true, 400, 0), b"\x1b[M\x60\xff\x21".to_vec());
    }
}
