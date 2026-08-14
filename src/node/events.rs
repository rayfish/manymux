//! Everything in a session's output stream that the screen model does not keep.
//!
//! `avt` reproduces the screen, but it models only a handful of terminal modes
//! (cursor keys, origin, autowrap, cursor visibility, alt screen) and throws
//! OSC payloads and BEL away. That is fine while you are attached, because the
//! bytes pass through to your terminal untouched. It is not fine on reattach,
//! where the repaint is rebuilt from the model: without this, coming back to a
//! session would leave the mouse dead, bracketed paste off, and the window
//! titled whatever it was before.
//!
//! So this scans the same stream in parallel and keeps what `avt` drops: the
//! title, the bells, and the modes to switch back on when someone reattaches.
//!
//! It is a real VT state machine rather than a substring search because the
//! same bytes mean different things in different states: `ESC ] 0 ; title BEL`
//! sets a title and must not count as a bell.

/// What the scanner found in a chunk of output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The program set its terminal title (OSC 0, 1 or 2).
    Title(String),
    /// BEL in the ground state.
    Bell,
    /// A desktop notification request: OSC 9 (iTerm2/kitty style) or
    /// OSC 777 `notify;title;body`.
    Notify { title: String, body: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Saw ESC, waiting on the introducer.
    Escape,
    /// Inside a CSI sequence, collecting parameters and intermediates.
    Csi,
    /// Inside an OSC string, collecting the payload.
    Osc,
    /// Inside an OSC string, saw ESC and are waiting for `\` (ST).
    OscEscape,
    /// Inside a DCS/SOS/PM/APC string we don't care about.
    Ignored,
    /// Inside an ignored string, saw ESC.
    IgnoredEscape,
}

/// Private modes worth turning back on when a client reattaches.
///
/// Deliberately excludes the ones `avt`'s screen dump already restores (1, 6,
/// 7, 25, 1047, 1048, 1049): re-emitting those would fight the repaint.
/// Everything here is about *input* and how the terminal talks back, which is
/// exactly what the screen model has no opinion on.
///
/// Detaching undoes this list, in `client::attach`. The two have to stay
/// together: a mode switched on for a session and not switched off again is a
/// mode left on in the shell you get back.
/// Bracketed paste: the program has asked to be told when text arrives as a
/// paste rather than as typing. Named because a pasted file's path is wrapped
/// in it when the program is expecting that.
pub const BRACKETED_PASTE: u16 = 2004;

pub const REPLAYED_MODES: &[u16] = &[
    9,    // X10 mouse reporting
    66,   // application keypad
    1000, // mouse button press and release
    1002, // mouse drag tracking
    1003, // mouse motion tracking
    1004, // focus in/out reporting
    1005, // UTF-8 mouse encoding
    1006, // SGR mouse encoding
    1015, // urxvt mouse encoding
    1016, // SGR pixel mouse encoding
    BRACKETED_PASTE,
];

/// Longest CSI parameter run we buffer. Real sequences are a handful of bytes.
const MAX_CSI: usize = 64;

/// Longest OSC payload we buffer. Real titles are short; anything past this is
/// an image transfer or a paste, and we drop it rather than grow unboundedly.
const MAX_OSC: usize = 4096;

#[derive(Debug)]
pub struct Scanner {
    state: State,
    osc: Vec<u8>,
    /// Set when an OSC payload overflowed, so we discard it at the terminator
    /// instead of reporting a truncated title.
    overflowed: bool,
    /// Parameter and intermediate bytes of the CSI sequence in progress.
    csi: Vec<u8>,
    /// Private modes the program has switched on, in the order they were first
    /// seen, so a replay reproduces the program's own ordering.
    modes: Vec<u16>,
    /// Last cursor style set with DECSCUSR (`CSI Ps SP q`).
    cursor_style: Option<u16>,
    /// Last title the program set. Kept here as well as on the session because
    /// a reattach has to restore it on the client's terminal.
    title: Option<String>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            state: State::Ground,
            osc: Vec::new(),
            overflowed: false,
            csi: Vec::new(),
            modes: Vec::new(),
            cursor_style: None,
            title: None,
        }
    }

    /// The escape sequences that put a freshly attached terminal back into the
    /// state the program expects, for everything the screen dump does not
    /// cover: the title, mouse reporting, bracketed paste, cursor style.
    ///
    /// Without this, reattaching to a full-screen program leaves the mouse
    /// dead until the program happens to re-enable it.
    pub fn replay(&self) -> String {
        let mut out = String::new();
        if let Some(title) = &self.title {
            out.push_str(&format!("\x1b]2;{title}\x07"));
        }
        for mode in &self.modes {
            out.push_str(&format!("\x1b[?{mode}h"));
        }
        if let Some(style) = self.cursor_style {
            out.push_str(&format!("\x1b[{style} q"));
        }
        out
    }

    /// The title the program last set, if any.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Whether the program is expecting pastes to be bracketed. A path pasted
    /// into a shell that asked for this must arrive as a paste, or the shell
    /// runs it the moment a newline turns up in the middle of it.
    pub fn bracketed_paste(&self) -> bool {
        self.modes.contains(&BRACKETED_PASTE)
    }

    /// Feed a chunk of output. Partial sequences are carried across calls, so
    /// chunk boundaries never split a title.
    pub fn feed(&mut self, chunk: &[u8], mut out: impl FnMut(Event)) {
        for &b in chunk {
            match (self.state, b) {
                (State::Ground, 0x07) => out(Event::Bell),
                (State::Ground, 0x1b) => self.state = State::Escape,
                (State::Ground, _) => {}

                (State::Escape, b'[') => {
                    self.state = State::Csi;
                    self.csi.clear();
                }
                (State::Escape, b']') => {
                    self.state = State::Osc;
                    self.osc.clear();
                    self.overflowed = false;
                }
                // DCS, SOS, PM, APC: string sequences terminated by ST.
                (State::Escape, b'P' | b'X' | b'^' | b'_') => self.state = State::Ignored,
                // A second ESC restarts the escape; anything else ends it. We
                // don't need to model the parameter bytes of CSI here because
                // no CSI sequence can contain a bare BEL.
                (State::Escape, 0x1b) => {}
                (State::Escape, _) => self.state = State::Ground,

                // xterm executes C0 controls inside a CSI rather than
                // aborting, so a bell here is still a bell.
                (State::Csi, 0x07) => out(Event::Bell),
                // Parameter (0x30-0x3f) and intermediate (0x20-0x2f) bytes.
                (State::Csi, 0x20..=0x3f) => {
                    if self.csi.len() < MAX_CSI {
                        self.csi.push(b);
                    }
                }
                // A final byte ends the sequence.
                (State::Csi, 0x40..=0x7e) => {
                    self.state = State::Ground;
                    self.finish_csi(b);
                }
                (State::Csi, _) => {}

                (State::Osc, 0x07) => {
                    self.finish_osc(&mut out);
                }
                (State::Osc, 0x1b) => self.state = State::OscEscape,
                (State::Osc, _) => {
                    if self.osc.len() < MAX_OSC {
                        self.osc.push(b);
                    } else {
                        self.overflowed = true;
                    }
                }

                (State::OscEscape, b'\\') => self.finish_osc(&mut out),
                (State::OscEscape, _) => {
                    // Not a string terminator, so the ESC was payload.
                    self.state = State::Osc;
                    if self.osc.len() + 2 <= MAX_OSC {
                        self.osc.push(0x1b);
                        self.osc.push(b);
                    } else {
                        self.overflowed = true;
                    }
                }

                (State::Ignored, 0x1b) => self.state = State::IgnoredEscape,
                (State::Ignored, _) => {}
                (State::IgnoredEscape, b'\\') => self.state = State::Ground,
                (State::IgnoredEscape, 0x1b) => {}
                (State::IgnoredEscape, _) => self.state = State::Ignored,
            }
        }
    }

    /// Record the state-changing CSI sequences: private mode set/reset, and
    /// cursor style.
    fn finish_csi(&mut self, final_byte: u8) {
        let params = std::mem::take(&mut self.csi);

        // DECSCUSR: `CSI Ps SP q`.
        if final_byte == b'q' && params.last() == Some(&b' ') {
            let digits = &params[..params.len() - 1];
            self.cursor_style = std::str::from_utf8(digits)
                .ok()
                .and_then(|p| p.parse().ok());
            return;
        }

        // DECSET / DECRST: `CSI ? Ps ; Ps h` or `... l`.
        if !matches!(final_byte, b'h' | b'l') || params.first() != Some(&b'?') {
            return;
        }
        let enable = final_byte == b'h';
        let Ok(text) = std::str::from_utf8(&params[1..]) else {
            return;
        };
        for mode in text.split(';').filter_map(|p| p.parse::<u16>().ok()) {
            if !REPLAYED_MODES.contains(&mode) {
                continue;
            }
            self.modes.retain(|m| *m != mode);
            if enable {
                self.modes.push(mode);
            }
        }
    }

    fn finish_osc(&mut self, out: &mut impl FnMut(Event)) {
        self.state = State::Ground;
        let payload = std::mem::take(&mut self.osc);
        if self.overflowed {
            self.overflowed = false;
            return;
        }
        if let Some(event) = parse_osc(&payload) {
            if let Event::Title(title) = &event {
                self.title = Some(title.clone());
            }
            out(event);
        }
    }
}

fn parse_osc(payload: &[u8]) -> Option<Event> {
    let text = String::from_utf8_lossy(payload);
    let (ps, pt) = text.split_once(';')?;
    match ps {
        // 0 sets icon name and title, 1 icon name only, 2 title only. Treating
        // 1 as a title too matches what terminals show in a tab strip.
        "0" | "1" | "2" => Some(Event::Title(clean(pt))),
        // OSC 9 ; body
        "9" => Some(Event::Notify {
            title: String::new(),
            body: clean(pt),
        }),
        // OSC 777 ; notify ; title ; body
        "777" => {
            let rest = pt.strip_prefix("notify;")?;
            let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
            Some(Event::Notify {
                title: clean(title),
                body: clean(body),
            })
        }
        _ => None,
    }
}

/// Strip control characters so a title can't move the cursor when we print it
/// in `mm ls`, and cap the length.
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Incremental UTF-8 decoder.
///
/// `avt` takes `&str`, but PTY output arrives in arbitrary chunks that split
/// multi-byte characters. This holds the incomplete tail until the rest shows
/// up, and substitutes U+FFFD for genuinely invalid bytes so one bad byte can't
/// desynchronise the screen forever.
#[derive(Debug, Default)]
pub struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, chunk: &[u8]) -> String {
        let bytes: &[u8] = if self.pending.is_empty() {
            chunk
        } else {
            self.pending.extend_from_slice(chunk);
            &self.pending
        };

        let mut out = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    out.push_str(s);
                    rest = &[];
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    // SAFETY-free: from_utf8 already validated this prefix.
                    out.push_str(std::str::from_utf8(&rest[..valid]).unwrap());
                    match e.error_len() {
                        // Invalid bytes: emit a replacement and skip them.
                        Some(n) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            rest = &rest[valid + n..];
                        }
                        // Truncated at the end of the chunk: keep it for later.
                        None => {
                            rest = &rest[valid..];
                            break;
                        }
                    }
                }
            }
        }

        let tail = rest.to_vec();
        self.pending = tail;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(input: &[u8]) -> Vec<Event> {
        let mut s = Scanner::new();
        let mut events = Vec::new();
        s.feed(input, |e| events.push(e));
        events
    }

    #[test]
    fn bell_in_ground_state_counts() {
        assert_eq!(scan(b"hi\x07there"), vec![Event::Bell]);
    }

    #[test]
    fn osc_terminator_bell_is_not_a_bell() {
        assert_eq!(
            scan(b"\x1b]0;my title\x07"),
            vec![Event::Title("my title".into())]
        );
    }

    #[test]
    fn osc_accepts_st_terminator() {
        assert_eq!(
            scan(b"\x1b]2;other\x1b\\"),
            vec![Event::Title("other".into())]
        );
    }

    #[test]
    fn sequences_split_across_chunks() {
        let mut s = Scanner::new();
        let mut events = Vec::new();
        s.feed(b"\x1b]0;split ti", |e| events.push(e));
        assert!(events.is_empty());
        s.feed(b"tle\x07\x07", |e| events.push(e));
        assert_eq!(
            events,
            vec![Event::Title("split title".into()), Event::Bell]
        );
    }

    #[test]
    fn osc_777_notification() {
        assert_eq!(
            scan(b"\x1b]777;notify;Claude;done thinking\x07"),
            vec![Event::Notify {
                title: "Claude".into(),
                body: "done thinking".into(),
            }]
        );
    }

    #[test]
    fn dcs_payload_cannot_ring_the_bell() {
        // A BEL inside a DCS string is payload, not a bell.
        assert_eq!(scan(b"\x1bPq\x07data\x1b\\\x07"), vec![Event::Bell]);
    }

    #[test]
    fn oversized_osc_is_dropped_not_truncated() {
        let mut input = b"\x1b]0;".to_vec();
        input.extend(std::iter::repeat_n(b'x', MAX_OSC + 100));
        input.push(0x07);
        assert_eq!(scan(&input), vec![]);
    }

    fn scanner_after(input: &[u8]) -> Scanner {
        let mut s = Scanner::new();
        s.feed(input, |_| {});
        s
    }

    #[test]
    fn mouse_and_paste_modes_are_replayed_on_reattach() {
        // What a full-screen program enables on startup.
        let s = scanner_after(b"\x1b[?1002h\x1b[?1006h\x1b[?2004h");
        assert_eq!(s.replay(), "\x1b[?1002h\x1b[?1006h\x1b[?2004h");
    }

    #[test]
    fn a_mode_the_program_turned_off_is_not_replayed() {
        let s = scanner_after(b"\x1b[?1002h\x1b[?2004h\x1b[?1002l");
        assert_eq!(s.replay(), "\x1b[?2004h");
    }

    #[test]
    fn modes_the_screen_dump_already_restores_are_left_alone() {
        // Re-emitting the alt screen or cursor visibility would fight `avt`.
        let s = scanner_after(b"\x1b[?1049h\x1b[?25l\x1b[?7h\x1b[?1h");
        assert_eq!(s.replay(), "");
    }

    #[test]
    fn one_sequence_can_set_several_modes() {
        let s = scanner_after(b"\x1b[?1000;1006;2004h");
        assert_eq!(s.replay(), "\x1b[?1000h\x1b[?1006h\x1b[?2004h");
    }

    #[test]
    fn the_title_leads_the_replay() {
        let s = scanner_after(b"\x1b]0;fixing the parser\x07\x1b[?1006h");
        assert_eq!(s.title(), Some("fixing the parser"));
        assert_eq!(s.replay(), "\x1b]2;fixing the parser\x07\x1b[?1006h");
    }

    #[test]
    fn cursor_style_survives_a_reattach() {
        let s = scanner_after(b"\x1b[5 q");
        assert_eq!(s.replay(), "\x1b[5 q");
    }

    #[test]
    fn a_csi_sequence_does_not_swallow_the_next_bell() {
        // Mouse input and cursor moves are constant traffic; the state machine
        // has to come back to ground cleanly after each one.
        assert_eq!(scan(b"\x1b[<0;10;5M\x1b[1;1H\x07"), vec![Event::Bell]);
    }

    #[test]
    fn a_mode_is_replayed_once_however_often_it_is_set() {
        let s = scanner_after(b"\x1b[?1006h\x1b[?1006h\x1b[?1006h");
        assert_eq!(s.replay(), "\x1b[?1006h");
    }

    #[test]
    fn utf8_split_across_chunks() {
        let mut d = Utf8Decoder::new();
        let s = "héllo ✓";
        let bytes = s.as_bytes();
        let mut out = String::new();
        // Feed one byte at a time, the worst case for multi-byte characters.
        for b in bytes {
            out.push_str(&d.decode(&[*b]));
        }
        assert_eq!(out, s);
    }

    #[test]
    fn invalid_utf8_becomes_replacement() {
        let mut d = Utf8Decoder::new();
        assert_eq!(d.decode(b"a\xffb"), "a\u{fffd}b");
    }
}
