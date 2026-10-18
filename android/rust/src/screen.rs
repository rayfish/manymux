//! The session's screen, emulated on this side of the wire.
//!
//! On the desktop the terminal in front of somebody is the emulator, and the
//! client is a pipe between it and the node. An app has no such terminal, so
//! the bytes have to become a grid somewhere, and that somewhere is here: `avt`
//! is fed every byte and the app asks, once a frame, which rows changed.
//!
//! Once a frame and not once a byte. Output arriving faster than the screen is
//! drawn coalesces in the emulator on its own, which is the backpressure: bytes
//! pile into a grid of a fixed size rather than into a queue, and one call
//! collapses however many arrived.
//! A recreated view asks for every row again, including while disconnected;
//! this redraw uses the screen already held here and needs no host response.
//!
//! Cursor keys follow the emulator's application mode, including wheel
//! gestures translated into keys on the alternate screen.

use std::collections::BTreeSet;

use avt::{Cell, Vt};
use manymux::proto::Size;
use manymux::utf8::Utf8Decoder;

use crate::mouse::Tracking;

/// What the screen holds behind the visible rows.
///
/// Nothing. The node keeps the real history and hands over a window of it when
/// somebody scrolls, so a second copy here would buy a phone nothing and cost
/// it everything: `avt`'s default is an unbounded scrollback, and a session
/// left attached for a week to something that prints would grow it until the
/// app is killed for it.
const BEHIND: usize = 0;

/// An emulator of the app's own, and the rows of it the app has not been
/// given since they last changed.
///
/// Both surfaces are one of these: the session's screen, and the history view
/// painted into a grid the same shape ([`crate::scroll`]). So both are built
/// with no scrollback and both hand rows over through [`runs_of`].
pub(crate) struct Painted {
    vt: Vt,
    changed: BTreeSet<usize>,
    size: Size,
}

impl Painted {
    pub(crate) fn at(size: Size) -> Self {
        Self {
            vt: Vt::builder()
                .size(size.cols as usize, size.rows as usize)
                .scrollback_limit(BEHIND)
                .build(),
            changed: BTreeSet::new(),
            size,
        }
    }

    pub(crate) fn size(&self) -> Size {
        self.size
    }

    pub(crate) fn feed(&mut self, text: &str) {
        let changes = self.vt.feed_str(text);
        self.changed.extend(changes.lines.iter().copied());
    }

    /// Reflowed to a new shape, with every row owed: each is somewhere else
    /// now, whatever the emulator says moved.
    pub(crate) fn resize(&mut self, size: Size) {
        self.size = size;
        self.vt.resize(size.cols as usize, size.rows as usize);
        self.mark_all();
    }

    /// Every row is owed, whatever the emulator thinks changed.
    pub(crate) fn mark_all(&mut self) {
        self.changed.extend(0..self.size.rows as usize);
    }

    /// The rows that changed since this was last asked, in order.
    pub(crate) fn take_rows(&mut self) -> Vec<Row> {
        std::mem::take(&mut self.changed)
            .into_iter()
            .filter(|at| *at < self.size.rows as usize)
            .map(|at| Row {
                at: at as u16,
                runs: runs_of(self.vt.line(at)),
            })
            .collect()
    }
}

/// One session's screen.
pub struct Screen {
    painted: Painted,
    decoding: Utf8Decoder,
    /// Whether the program in there is reading the mouse itself, which is the
    /// one thing about the session's output that `avt` throws away and a drag
    /// needs. Reset with the rest of it on a repaint, the modes being replayed
    /// with the screen they belong to.
    mouse: Tracking,
}

/// What changed since the last frame.
#[derive(uniffi::Record)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cursor: Cursor,
    /// Only the rows that changed, in order. A frame with none of them is a
    /// frame the app can skip drawing entirely.
    pub changed: Vec<Row>,
}

/// Where the cursor is, in cells.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
}

/// One row of the screen, as runs of a single pen.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct Row {
    pub at: u16,
    pub runs: Vec<Run>,
}

/// A stretch of one row that looks the same all the way along.
///
/// Runs rather than cells: a row of 45 cells is a handful of these, and the
/// widget draws each with one call.
#[derive(Debug, PartialEq, Eq, uniffi::Record)]
pub struct Run {
    pub text: String,
    /// How many cells wide, which is not the number of characters: a wide
    /// character is two. The widget advances by this and never by the font's
    /// own idea of the text's width, or one CJK character puts the rest of the
    /// row out of step with the grid.
    pub cells: u16,
    pub look: Look,
}

/// How a run is painted.
#[derive(Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct Look {
    pub foreground: Colour,
    pub background: Colour,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub blink: bool,
    pub inverse: bool,
}

/// A colour, or the absence of one.
#[derive(Debug, Default, PartialEq, Eq, uniffi::Enum)]
pub enum Colour {
    /// Whatever the app's theme says text or background is. Kept apart from an
    /// explicit colour because a theme can be light or dark and a session that
    /// asked for neither must follow it.
    #[default]
    Default,
    /// One of the terminal palette's, which the app resolves.
    Indexed {
        index: u8,
    },
    Rgb {
        red: u8,
        green: u8,
        blue: u8,
    },
}

impl Screen {
    pub fn at(size: Size) -> Self {
        Self {
            painted: Painted::at(size),
            decoding: Utf8Decoder::default(),
            mouse: Tracking::default(),
        }
    }

    /// Bytes the session printed.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Before the decoder rather than after it: a mode sequence is ASCII,
        // and the raw bytes are what a chunk boundary falls in the middle of.
        self.mouse.feed(bytes);
        let text = self.decoding.decode(bytes);
        self.painted.feed(&text);
    }

    /// A repaint: whatever was on the screen before it is gone.
    ///
    /// `avt`'s dump emits no clear, no reset and no home. It paints from
    /// wherever the cursor is and does not write the blank rows at the bottom
    /// at all, so a repaint fed to the screen that was there before it leaves
    /// the old screen showing through wherever the new one has nothing. The
    /// emulator is therefore built again from nothing, and every row is
    /// reported changed: the app is holding a picture that has stopped being
    /// true, including the rows the dump says nothing about.
    pub fn repaint(&mut self, bytes: &[u8]) {
        *self = Self::at(self.painted.size);
        self.feed(bytes);
        self.painted.mark_all();
    }

    /// What the session has asked to be told about the mouse.
    pub fn mouse(&self) -> &Tracking {
        &self.mouse
    }

    pub fn cursor_key(&self, final_byte: u8) -> Vec<u8> {
        vec![
            0x1b,
            if self.painted.vt.cursor_key_app_mode() {
                b'O'
            } else {
                b'['
            },
            final_byte,
        ]
    }

    pub fn paste(&self, text: &str) -> Vec<u8> {
        if self.mouse.bracketed_paste() {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
        } else {
            text.as_bytes().to_vec()
        }
    }

    /// The session settled on a different size.
    pub fn resize(&mut self, size: Size) {
        self.painted.resize(size);
    }

    /// Mark the whole screen for a view that has no previous frame.
    pub fn redraw(&mut self) {
        self.painted.mark_all();
    }

    /// The rows that have changed since this was last asked, and the cursor.
    pub fn take_frame(&mut self) -> Frame {
        let cursor = self.painted.vt.cursor();
        let changed = self.painted.take_rows();
        let size = self.painted.size;

        Frame {
            cols: size.cols,
            rows: size.rows,
            cursor: Cursor {
                // Clamped, because `avt` lets the column sit one past the last
                // one while a wrap is pending, and the widget would draw the
                // block in a column the screen has not got.
                col: (cursor.col as u16).min(size.cols.saturating_sub(1)),
                row: cursor.row as u16,
                visible: cursor.visible,
            },
            changed,
        }
    }
}

/// Break a line into stretches that look the same.
///
/// Both surfaces the app draws come through here by way of [`Painted`], so a
/// colour or a wide character cannot come out one way on the screen and
/// another way in the lines above it.
fn runs_of(line: &avt::Line) -> Vec<Run> {
    // The predicate says where to *break*, not where to carry on: read the
    // other way round every cell is a run of its own, which draws the same
    // screen one call per character and loses the run boundaries the widget
    // is built on.
    line.chunks(|one, next| one.pen() != next.pen())
        .map(|cells| Run {
            text: cells.iter().map(Cell::char).collect(),
            cells: cells.iter().map(|cell| cell.width() as u16).sum(),
            look: look_of(&cells),
        })
        .collect()
}

fn look_of(cells: &[Cell]) -> Look {
    let Some(pen) = cells.first().map(Cell::pen) else {
        return Look::default();
    };
    Look {
        foreground: colour_of(pen.foreground()),
        background: colour_of(pen.background()),
        bold: pen.is_bold(),
        faint: pen.is_faint(),
        italic: pen.is_italic(),
        underline: pen.is_underline(),
        strikethrough: pen.is_strikethrough(),
        blink: pen.is_blink(),
        inverse: pen.is_inverse(),
    }
}

fn colour_of(colour: Option<avt::Color>) -> Colour {
    match colour {
        None => Colour::Default,
        Some(avt::Color::Indexed(index)) => Colour::Indexed { index },
        Some(avt::Color::RGB(rgb)) => Colour::Rgb {
            red: rgb.r,
            green: rgb.g,
            blue: rgb.b,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{Colour, Screen};
    use manymux::proto::Size;

    fn screen(cols: u16, rows: u16) -> Screen {
        Screen::at(Size::new(cols, rows))
    }

    /// What a row reads as, for the tests that do not care how it is broken up.
    fn text_of(screen: &mut Screen, at: u16) -> String {
        let frame = screen.take_frame();
        frame
            .changed
            .iter()
            .find(|row| row.at == at)
            .map(|row| row.runs.iter().map(|run| run.text.as_str()).collect())
            .unwrap_or_default()
    }

    #[test]
    fn paste_obeys_the_programs_bracketing_mode() {
        let mut screen = Screen::at(Size::new(12, 4));
        assert_eq!(screen.paste("one\ntwo"), b"one\ntwo");
        screen.feed(b"\x1b[?2004h");
        assert_eq!(screen.paste("one\ntwo"), b"\x1b[200~one\ntwo\x1b[201~");
        assert_eq!(screen.paste("\x1b[201~"), b"\x1b[200~[201~\x1b[201~");
    }

    #[test]
    fn cursor_keys_follow_application_mode_after_repaint() {
        let mut screen = Screen::at(Size::new(12, 4));
        assert_eq!(screen.cursor_key(b'A'), b"\x1b[A");
        screen.feed(b"\x1b[?1h");
        assert_eq!(screen.cursor_key(b'B'), b"\x1bOB");
        screen.repaint(b"\x1b[?1h\x1b[?1049h");
        assert!(screen.mouse().alternate_scroll());
        assert_eq!(screen.cursor_key(b'A'), b"\x1bOA");
        screen.repaint(b"prompt");
        assert!(!screen.mouse().alternate_scroll());
        assert_eq!(screen.cursor_key(b'A'), b"\x1b[A");
    }

    #[test]
    fn what_the_session_printed_comes_back_as_a_row() {
        let mut screen = screen(20, 4);

        screen.feed(b"hello");

        assert_eq!(text_of(&mut screen, 0).trim_end(), "hello");
    }

    #[test]
    fn a_character_split_across_chunks_is_one_character() {
        let mut screen = screen(20, 4);

        // The worst case, and the ordinary one: a DATA frame ends wherever the
        // read did, which is as likely to be inside a character as anywhere.
        for byte in "héllo ✓".as_bytes() {
            screen.feed(&[*byte]);
        }

        assert_eq!(text_of(&mut screen, 0).trim_end(), "héllo ✓");
    }

    #[test]
    fn only_the_rows_that_changed_come_back() {
        let mut screen = screen(20, 4);
        screen.feed(b"one\r\ntwo\r\nthree");
        screen.take_frame();

        screen.feed(b" more");

        let frame = screen.take_frame();
        let rows: Vec<u16> = frame.changed.iter().map(|row| row.at).collect();
        assert_eq!(rows, vec![2]);
    }

    #[test]
    fn a_frame_with_nothing_in_it_is_a_frame_to_skip() {
        let mut screen = screen(20, 4);
        screen.feed(b"hello");
        screen.take_frame();

        let frame = screen.take_frame();

        assert!(frame.changed.is_empty());
    }

    #[test]
    fn a_new_view_can_redraw_without_more_output() {
        let mut screen = screen(20, 4);
        screen.feed(b"hello");
        let original = screen.take_frame();
        assert!(screen.take_frame().changed.is_empty());

        screen.redraw();
        let redrawn = screen.take_frame();
        assert_eq!(redrawn.changed.len(), 4);
        assert_eq!(
            redrawn.changed[0].runs[0].text,
            original.changed[0].runs[0].text
        );
        assert_eq!(redrawn.cursor.col, original.cursor.col);
        assert!(screen.take_frame().changed.is_empty());
    }

    #[test]
    fn a_repaint_leaves_nothing_of_the_screen_before_it() {
        let mut screen = screen(20, 4);
        screen.feed(b"one\r\ntwo\r\nthree\r\nfour");
        screen.take_frame();

        // What the node sends on attach: `avt`'s own dump, which pads the rows
        // it paints and says nothing at all about the ones below them, from
        // wherever the cursor happens to be.
        let mut elsewhere = avt::Vt::builder().size(20, 4).build();
        elsewhere.feed_str("fresh");
        screen.repaint(elsewhere.dump().as_bytes());

        let frame = screen.take_frame();
        // Every row, because the app is holding a picture of a screen that has
        // been replaced, including the rows the dump says nothing about.
        assert_eq!(frame.changed.len(), 4);
        let shown: Vec<String> = frame
            .changed
            .iter()
            .map(|row| {
                row.runs
                    .iter()
                    .map(|run| run.text.as_str())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        // The far end's screen exactly, and not a word of the one before it.
        assert_eq!(shown, vec!["fresh", "", "", ""]);
    }

    #[test]
    fn a_pen_the_session_set_comes_through() {
        let mut screen = screen(20, 4);

        screen.feed(b"plain\x1b[1;31mloud\x1b[0m");

        let frame = screen.take_frame();
        let runs = &frame.changed[0].runs;
        let loud = runs
            .iter()
            .find(|run| run.text.starts_with("loud"))
            .expect("the run that was written in red");
        assert!(loud.look.bold);
        assert_eq!(loud.look.foreground, Colour::Indexed { index: 1 });
        let plain = runs
            .iter()
            .find(|run| run.text.starts_with("plain"))
            .expect("the run before it");
        assert!(!plain.look.bold);
        assert_eq!(plain.look.foreground, Colour::Default);
    }

    #[test]
    fn a_wide_character_is_two_cells_and_one_character() {
        let mut screen = screen(20, 4);

        // A pen change after them, so the run ends where the wide characters do
        // rather than running on into the blanks.
        screen.feed("漢字\x1b[31mx".as_bytes());

        let frame = screen.take_frame();
        let first = &frame.changed[0].runs[0];
        assert_eq!(first.text, "漢字");
        // Four cells for two characters. A widget advancing by the text's own
        // width would put the rest of the row a column out per character.
        assert_eq!(first.cells, 4);
    }

    #[test]
    fn a_row_that_scrolled_away_is_never_reported() {
        let mut screen = screen(20, 4);

        for line in 0..100 {
            screen.feed(format!("line {line}\r\n").as_bytes());
        }

        let frame = screen.take_frame();
        assert!(
            frame.changed.iter().all(|row| row.at < 4),
            "a row off the bottom of a four row screen: {:?}",
            frame.changed.iter().map(|row| row.at).collect::<Vec<_>>()
        );
    }
}
