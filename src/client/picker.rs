//! The list control mode puts on the screen.
//!
//! A box of rows with one highlighted, drawn over whatever the session last
//! painted. Two things fill it: the sessions you can reach, and the groups you
//! can put one in. One widget rather than two, because the movement keys are
//! the same and the columns differ only in what goes in them.
//!
//! Terminal-free, like [`super::status`]: it answers with a `String` of escape
//! sequences and never touches stdout, so a caller with no terminal can drive
//! it and the tests can read what it drew.
//!
//! The split with [`super::attach::keys`] is the one [`super::scroll`] already
//! uses: the key filter says which way, and this says where that lands. A key
//! table that also knew how many rows there were would be a key table that had
//! to be told about listings.
//!
//! The cursor is a band and a mark rather than reverse video: a row is pointed
//! at, not repainted, so the name stays the brightest thing on it and the detail
//! stays the quieter one, which is how every other row in the box is read. The
//! ground is [`super::pen::BAND`], the one a selection in the history view sits
//! on, since both are one gesture saying "this one". Reverse video is what is
//! left where there is no palette to sit a band on, and it is why the mark is a
//! glyph as well as a colour. What the caller owes on the way up is the
//! terminal's own cursor: it is the session's, it sits wherever the session left
//! it, and under a box it blinks on a row that has nothing to do with this one.
//!
//! [`Row::id`] is opaque here on purpose. The caller hands rows in and gets an
//! id back, so this never learns what a host is, and a listing that lands while
//! the popup is up can be swapped in with [`Picker::replace`] without the
//! highlight sliding onto a different session.

use crate::proto::Size;
use crate::style;

/// Columns kept clear either side of the box, so it reads as something over the
/// screen rather than something welded to its edges.
const MARGIN: u16 = 2;

/// Rows the box spends on its frame: the title, the rule above the hints, the
/// hints, and the bottom edge.
const FURNITURE: u16 = 4;

/// The narrowest box worth drawing. Below this the columns collide and the
/// popup says less than the mark row does.
const MIN_COLS: u16 = 24;

/// The widest. A list is read down its left edge, and one stretched across a
/// wide terminal puts the note so far from the name that they stop reading as
/// one row.
const WIDEST: u16 = 60;

/// Columns a row moves in per level of the tree.
const STEP: u16 = 2;

/// Columns of gap inside a session row, and the width of the note pinned to its
/// right. What is left over after the name column is the detail's.
const GAPS: u16 = 6;
const NOTE: u16 = 6;

/// The mark on the row the cursor is on, in the first of the two columns every
/// row starts with. A glyph as well as a colour, so the cursor is still a mark
/// on a screen that is showing none.
const POINT: char = '▌';

/// What the two halves of the pointed-at row are drawn in, over the band.
///
/// Bright enough to read on it, and still one brighter than the other: a cursor
/// marks a row, it does not repaint it.
const NAME: &str = "1;38;5;255";
const REST: &str = "22;38;5;250";

/// The ground that row sits on: [`super::pen::BAND`], which is what a selection
/// in the history view sits on. The two gestures say the same thing, "this
/// one", and saying it two ways would be two vocabularies for one idea.
fn ground() -> String {
    format!("48;5;{}", super::pen::BAND)
}

/// What the name column is allowed to shrink and grow to.
///
/// It takes the widest name there is, between these: a list of bare session
/// names leaves the detail room to say what each one is doing, and one where
/// every row carries its machine takes the room to say that instead. Sized once
/// for the whole list rather than per row, because these are columns and a
/// column that moves down the box is not one.
const MIN_LABEL: u16 = 12;
const MAX_LABEL: u16 = 30;

/// One line of the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Whatever the caller needs to act on this row. Never read here.
    pub id: usize,
    /// Left column: the session's name, or the group's.
    pub label: String,
    /// Middle column, dim: what the session is doing, or how many sessions the
    /// group holds.
    pub detail: String,
    /// Right column: idle time and bells, or the mark on the row you are in.
    pub note: String,
    /// A heading rather than something you can land on: a group's name above
    /// the sessions in it, or a machine's above the ones in no group. Skipped
    /// by the highlight, since Enter on one would have nothing to attach to.
    pub heading: bool,
    /// How far in the row sits, in steps of [`STEP`] columns. The list is a
    /// tree two deep, and this is the only thing that says so.
    pub indent: u16,
}

impl Row {
    pub fn new(id: usize, label: impl Into<String>) -> Self {
        Self {
            id,
            label: label.into(),
            detail: String::new(),
            note: String::new(),
            heading: false,
            indent: 0,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = detail.into();
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    /// How far in this row sits. See [`Row::indent`].
    pub fn indent(mut self, indent: u16) -> Self {
        self.indent = indent;
        self
    }

    /// What a run of sessions sits under: a group's name, or a machine's.
    ///
    /// One constructor for both, because nothing here tells them apart: they
    /// are siblings in the list, they are skipped the same way, and `h` steps
    /// between them without caring which it landed on. What they are is the
    /// caller's business, and it says so in the label.
    pub fn heading(label: impl Into<String>) -> Self {
        Self {
            heading: true,
            ..Self::new(usize::MAX, label)
        }
    }
}

pub struct Picker {
    title: String,
    hints: String,
    rows: Vec<Row>,
    /// Which row is highlighted. Always a row that can be landed on, unless
    /// there are none at all.
    at: usize,
    /// The first row on screen, for a list longer than the box.
    top: usize,
    /// Where the box was last drawn, so a box that has since changed shape can
    /// clear what it used to cover.
    ///
    /// It has to clear it itself: the cells under a popup are the popup's, the
    /// session's screen there having been painted over when the box went up and
    /// put back by the resync that closing it asks for. Nothing else is going
    /// to, and a box that grew when the first real listing landed left the top
    /// of the old one sitting on the screen saying nothing.
    drawn: Option<(u16, u16)>,
}

impl Picker {
    pub fn new(
        title: impl Into<String>,
        hints: impl Into<String>,
        rows: Vec<Row>,
        at: usize,
    ) -> Self {
        let mut picker = Self {
            title: title.into(),
            hints: hints.into(),
            rows,
            at,
            top: 0,
            drawn: None,
        };
        // A caller that asked for a heading, or for a row past the end, gets
        // put somewhere it can act from rather than a popup whose Enter does
        // nothing.
        if picker.landable(picker.at).is_none() {
            picker.at = picker.first().unwrap_or(0);
        }
        picker
    }

    pub fn title(&self) -> &str {
        &self.title
    }

    /// The highlighted row, or none when there is nothing to land on.
    pub fn chosen(&self) -> Option<&Row> {
        self.landable(self.at)
    }

    pub fn up(&mut self) {
        self.step(false);
    }

    pub fn down(&mut self) {
        self.step(true);
    }

    /// Swap in a new listing without moving off the session you were on.
    ///
    /// By id rather than by index, because a row index means something
    /// different the moment a session ends: the highlight would slide onto its
    /// neighbour under a hand that had not moved.
    pub fn replace(&mut self, rows: Vec<Row>, at: usize) {
        let was = self.chosen().map(|row| row.id);
        self.rows = rows;
        self.at = was
            .and_then(|id| {
                self.rows
                    .iter()
                    .position(|row| row.id == id && !row.heading)
            })
            // Nothing was highlighted, which is a box that opened on an empty
            // listing: the fan-out takes longer than the half second the switch
            // keys give it, so on a slow fleet the *first* popup of a run always
            // opens with nothing in it. The caller's row is where it should land
            // then, for the same reason it lands there when a listing was ready:
            // the session you are in is the one you are looking out from, and
            // Enter is the next key. Falling back to the first row hopped you
            // into somebody else's session.
            .or_else(|| self.landable(at).is_some().then_some(at))
            .or_else(|| self.first())
            .unwrap_or(0);
        self.top = 0;
    }

    /// Put the highlight on the row with this id, if it is still here.
    ///
    /// For coming back to a list you left: the id is the caller's and outlives
    /// the picker that was showing it, while an index does not survive a
    /// listing landing in between.
    pub fn point_at(&mut self, id: usize) {
        if let Some(at) = self
            .rows
            .iter()
            .position(|row| row.id == id && !row.heading)
        {
            self.at = at;
        }
    }

    fn landable(&self, index: usize) -> Option<&Row> {
        self.rows.get(index).filter(|row| !row.heading)
    }

    fn first(&self) -> Option<usize> {
        self.rows.iter().position(|row| !row.heading)
    }

    /// The first row under the next heading, or the previous one.
    ///
    /// A bigger step of the same gesture, and it commits nothing, which Enter
    /// is for. A section rather than a machine, since a group is a heading here
    /// too and the list leads with them: the key steps to the next run of
    /// sessions, whatever gathered it.
    pub fn next_heading(&mut self, forwards: bool) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let mut seen_heading = false;
        let mut next = self.at;
        for _ in 0..len {
            next = if forwards {
                (next + 1) % len
            } else {
                (next + len - 1) % len
            };
            if self.rows[next].heading {
                seen_heading = true;
                continue;
            }
            // Going back, the row wanted is the first under the heading rather
            // than the last one above it, so keep walking to the top of the run.
            if seen_heading && (forwards || self.starts_a_run(next)) {
                self.at = next;
                return;
            }
        }
    }

    /// Whether this row is the first under its heading.
    fn starts_a_run(&self, index: usize) -> bool {
        index == 0 || self.rows[index - 1].heading
    }

    /// One row on, skipping headings and wrapping at both ends.
    ///
    /// Bounded by the row count rather than looping until it finds one: a list
    /// of nothing but headings has nowhere to go and must not spin.
    fn step(&mut self, forwards: bool) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let mut next = self.at;
        for _ in 0..len {
            next = if forwards {
                (next + 1) % len
            } else {
                (next + len - 1) % len
            };
            if self.landable(next).is_some() {
                self.at = next;
                return;
            }
        }
    }

    /// The whole popup, ready to write in one go.
    ///
    /// One string rather than a write per row, for the same reason
    /// `Status::setup` is one: there is never a frame showing half a box.
    pub fn draw(&mut self, size: Size) -> String {
        let Some(shape) = self.shape(size) else {
            return String::new();
        };
        let mut out = String::from("\x1b7");
        let layout = Layout {
            width: shape.inner,
            label: self.label_width(shape.inner),
            colour: style::coloured(),
        };
        let width = layout.width;
        let height = u16::try_from(shape.visible).unwrap_or(u16::MAX) + FURNITURE;
        out.push_str(&self.cleared(&shape, height));
        self.drawn = Some((shape.row, height));
        let mut line = shape.row;
        let mut put = |line: u16, text: &str| {
            let mut piece = at(line, shape.col);
            piece.push_str(text);
            out.push_str(&piece);
        };

        // The title sits in the rule rather than on a row of blanks. Padded
        // with spaces it read as an unfinished edge next to the solid one above
        // the hints, which is the same box drawn two ways.
        put(
            line,
            &style::faint(&format!("┌{}┐", rule(&format!(" {} ", self.title), width))),
        );
        line += 1;

        for index in shape.top..shape.top + shape.visible {
            let painted = match self.rows.get(index) {
                // A short list still gets the box it asked for, so the frame
                // does not jump as rows come and go under a listing.
                None => style::faint(&" ".repeat(usize::from(width))),
                Some(row) => self.paint(row, layout, index == self.at),
            };
            put(
                line,
                &format!("{}{painted}{}", style::faint("│"), style::faint("│")),
            );
            line += 1;
        }

        put(
            line,
            &style::faint(&format!("├{}┤", "─".repeat(usize::from(width)))),
        );
        line += 1;
        put(
            line,
            &style::faint(&format!("│{}│", fit(&format!(" {} ", self.hints), width))),
        );
        line += 1;
        put(
            line,
            &style::faint(&format!("└{}┘", "─".repeat(usize::from(width)))),
        );
        out.push_str("\x1b8");
        out
    }

    /// Blanks over the rows the last box covered that this one will not.
    ///
    /// Only the rows it gives up, never the ones it is about to paint: blanking
    /// under the new box would be writing every cell twice and inviting the
    /// flicker that the view had. The columns never change under a box that has
    /// not moved sideways, and it only can when the terminal is resized, which
    /// repaints everything anyway.
    fn cleared(&self, shape: &Shape, height: u16) -> String {
        let Some((was, was_high)) = self.drawn else {
            return String::new();
        };
        let blank = " ".repeat(usize::from(shape.inner + 2));
        (was..was.saturating_add(was_high))
            .filter(|line| *line < shape.row || *line >= shape.row + height)
            .map(|line| format!("{}{blank}", at(line, shape.col)))
            .collect()
    }

    /// How wide the name column is for this list. See [`MIN_LABEL`].
    fn label_width(&self, width: u16) -> u16 {
        let widest = self
            .rows
            .iter()
            .filter(|row| !row.heading)
            .map(|row| columns(&row.label) + row.indent * STEP)
            .max()
            .unwrap_or(MIN_LABEL);
        widest
            .clamp(MIN_LABEL, MAX_LABEL)
            .min(width.saturating_sub(GAPS + NOTE))
    }

    /// One row, exactly `layout.width` columns wide.
    ///
    /// Every column is fixed before anything is written, because a row that
    /// sizes itself to its contents is a box with a ragged edge, and the edge is
    /// the only thing telling you where the popup stops and the session behind
    /// it starts. Which holds for the row under the cursor too, whichever of
    /// its two spellings it is drawn in: [`POINT`] takes one of the two columns
    /// a row starts with rather than being put in front of them.
    fn paint(&self, row: &Row, layout: Layout, here: bool) -> String {
        // The tree, and the whole of it: a row says where it sits by how far in
        // it starts, and nothing else in the box draws the shape.
        let inset = " ".repeat(usize::from(row.indent * STEP));
        if row.heading {
            let label = fit(&format!("{inset}{}", row.label), layout.width - 2);
            return format!("  {}", style::host(&label));
        }

        // The note is pinned to the right. The detail gives way first and the
        // label is clipped last: which session it is matters more than what it
        // is doing.
        let detail_width = layout.width.saturating_sub(GAPS + NOTE + layout.label);
        // Inside the label's column rather than in front of it, so the detail
        // and the note stay in line down the box however deep the row is.
        let label = fit(&format!("{inset}{}", row.label), layout.label);
        let detail = fit(&row.detail, detail_width);
        let note = right(&row.note, NOTE);

        if !here {
            return format!(
                "  {} {}  {} ",
                style::bold(&label),
                style::faint(&detail),
                style::faint(&note)
            );
        }
        if !layout.colour {
            // Reverse video, which is the one way of pointing at a row that
            // needs no palette at all: `NO_COLOR`, and anything that is not a
            // terminal, both land here.
            return format!("\x1b[7m  {label} {detail}  {note} \x1b[27m");
        }
        // The band, and the row lifted onto it rather than repainted: the name
        // is still the brightest thing on the line and the detail is still the
        // quieter one, which is the hierarchy every other row is drawn with and
        // the one thing reverse video threw away. One run of SGR per column and
        // no reset until the end, since a reset would take the ground with it.
        format!(
            "\x1b[0;{ground}m\x1b[{NAME}m{POINT} {label} \x1b[{REST}m{detail}  {note} \x1b[0m",
            ground = ground(),
        )
    }

    /// Where the box goes and how much of the list fits, or none when the
    /// terminal is too small to hold one worth drawing.
    fn shape(&self, size: Size) -> Option<Shape> {
        let inner = size
            .cols
            .saturating_sub(MARGIN * 2 + 2)
            .clamp(MIN_COLS, WIDEST);
        if inner + MARGIN * 2 + 2 > size.cols || size.rows < FURNITURE + 3 {
            return None;
        }
        let room = size.rows.saturating_sub(FURNITURE + 2);
        let visible = usize::from(room).min(self.rows.len()).max(1);
        // Scrolled just far enough to keep the highlight on screen, rather than
        // centred: a list that slides under every keypress is harder to read
        // than one that holds still until it has to move.
        let top = if self.at < visible {
            0
        } else {
            self.at + 1 - visible
        };
        let height = u16::try_from(visible).unwrap_or(u16::MAX) + FURNITURE;
        let row = (size.rows.saturating_sub(height)) / 2 + 1;
        let col = (size.cols.saturating_sub(inner + 2)) / 2 + 1;
        Some(Shape {
            row,
            col,
            inner,
            top,
            visible,
        })
    }
}

/// The measurements every row of one drawing shares.
///
/// Worked out once for the whole box, like [`Picker::label_width`] and for the
/// same reason: a column that moved down the box would not be a column.
#[derive(Debug, Clone, Copy)]
struct Layout {
    /// Columns between the two vertical edges.
    width: u16,
    /// What the name column was sized to.
    label: u16,
    /// Whether the cursor can be a band. See [`Picker::paint`].
    colour: bool,
}

struct Shape {
    row: u16,
    col: u16,
    /// Columns between the two vertical edges.
    inner: u16,
    top: usize,
    visible: usize,
}

fn at(row: u16, col: u16) -> String {
    format!("\x1b[{row};{col}H")
}

fn columns(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

/// Cut to `width` columns, with an ellipsis where something was dropped.
///
/// Clipped rather than wrapped, because a box that grows a line when a title
/// gets longer is a box that moves under the hand reading it.
fn clip(text: &str, width: u16) -> String {
    let width = usize::from(width);
    if text.chars().count() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Exactly `width` columns: clipped if too long, padded on the right if short.
fn fit(text: &str, width: u16) -> String {
    let text = clip(text, width);
    let short = usize::from(width.saturating_sub(columns(&text)));
    format!("{text}{}", " ".repeat(short))
}

/// The same, but what is left over is the box's own edge rather than blank, so
/// a title sits in the rule instead of in a gap in it.
fn rule(text: &str, width: u16) -> String {
    let text = clip(text, width);
    let short = usize::from(width.saturating_sub(columns(&text)));
    format!("{text}{}", "─".repeat(short))
}

/// The same, padded on the left instead.
fn right(text: &str, width: u16) -> String {
    let text = clip(text, width);
    let short = usize::from(width.saturating_sub(columns(&text)));
    format!("{}{text}", " ".repeat(short))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picker(rows: Vec<Row>, at: usize) -> Picker {
        Picker::new("sessions", "⏎ go", rows, at)
    }

    /// What the popup drew, with the styling and the cursor moves taken out, so
    /// a test can read it the way a person would.
    fn seen(picker: &mut Picker, size: Size) -> Vec<String> {
        let drawn = picker.draw(size);
        let mut lines = Vec::new();
        let mut plain = String::new();
        let mut chars = drawn.chars().peekable();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                plain.push(c);
                continue;
            }
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut seq = String::new();
                    for c in chars.by_ref() {
                        seq.push(c);
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    if seq.ends_with('H') && !plain.is_empty() {
                        lines.push(std::mem::take(&mut plain));
                    }
                }
                // The cursor save and restore around the whole thing.
                _ => {
                    chars.next();
                }
            }
        }
        if !plain.is_empty() {
            lines.push(plain);
        }
        lines
    }

    const BIG: Size = Size { cols: 80, rows: 24 };

    /// A row's styling taken off, so a test can read what a person sees.
    fn plain(text: &str) -> String {
        let mut out = String::new();
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                out.push(c);
                continue;
            }
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        }
        out
    }

    fn row_of(picker: &Picker, colour: bool, here: bool) -> String {
        let layout = Layout {
            width: 40,
            label: MIN_LABEL,
            colour,
        };
        picker.paint(&picker.rows[0], layout, here)
    }

    fn one_row() -> Picker {
        picker(vec![Row::new(0, "build").detail("cargo").note("2m")], 0)
    }

    /// The cursor marks the row, it does not repaint it: reverse video did
    /// both, and the row under it was the one row in the box that could not say
    /// which half of itself was the name.
    #[test]
    fn the_row_under_the_cursor_sits_on_a_band_and_wears_the_mark() {
        let drawn = row_of(&one_row(), true, true);
        assert!(drawn.contains(&ground()), "no band: {drawn:?}");
        assert!(
            plain(&drawn).starts_with(POINT),
            "no mark: {:?}",
            plain(&drawn)
        );
        assert!(
            drawn.contains(NAME) && drawn.contains(REST),
            "the name and the rest are drawn the same: {drawn:?}"
        );
    }

    /// With no palette to sit a band on there is still one way of pointing at a
    /// row that every terminal has.
    #[test]
    fn a_terminal_with_no_colours_gets_the_cursor_in_reverse_video() {
        let drawn = row_of(&one_row(), false, true);
        assert!(drawn.starts_with("\x1b[7m"), "{drawn:?}");
        assert!(drawn.ends_with("\x1b[27m"), "{drawn:?}");
    }

    /// Whichever way it is spelt, or the box has a ragged edge, and the edge is
    /// the only thing saying where the popup stops and the session starts.
    #[test]
    fn the_cursor_row_is_as_wide_as_an_ordinary_one() {
        let picker = one_row();
        let ordinary = plain(&row_of(&picker, true, false)).chars().count();
        for colour in [true, false] {
            assert_eq!(
                plain(&row_of(&picker, colour, true)).chars().count(),
                ordinary,
                "the cursor row is a different width with colour {colour}"
            );
        }
    }

    #[test]
    fn the_highlight_wraps_at_both_ends() {
        let mut p = picker(vec![Row::new(0, "a"), Row::new(1, "b")], 0);
        p.up();
        assert_eq!(p.chosen().unwrap().id, 1);
        p.down();
        assert_eq!(p.chosen().unwrap().id, 0);
    }

    /// A machine's name is a label, not a place to land: stopping on one would
    /// leave Enter with nothing to attach to.
    #[test]
    fn a_heading_cannot_be_landed_on() {
        let rows = vec![
            Row::heading("box"),
            Row::new(0, "a"),
            Row::heading("gpu"),
            Row::new(1, "b"),
        ];
        let mut p = picker(rows, 1);
        p.down();
        assert_eq!(p.chosen().unwrap().id, 1, "straight over the heading");
        p.down();
        assert_eq!(p.chosen().unwrap().id, 0, "and around the end");
        p.up();
        assert_eq!(p.chosen().unwrap().id, 1);
    }

    /// A list of nothing but headings has nowhere to go, and stepping through
    /// it must stop rather than spin looking for a row that is not there.
    #[test]
    fn a_list_of_nothing_but_headings_has_nothing_to_choose() {
        let mut p = picker(vec![Row::heading("box")], 0);
        assert!(p.chosen().is_none());
        p.down();
        p.up();
        assert!(p.chosen().is_none());
    }

    #[test]
    fn opening_on_a_heading_lands_on_something_that_can_be_chosen() {
        let p = picker(vec![Row::heading("box"), Row::new(7, "a")], 0);
        assert_eq!(p.chosen().unwrap().id, 7);
    }

    /// A listing landing under the popup must not move the highlight off the
    /// session it was on, which a row index would do the moment one ended.
    #[test]
    fn replacing_the_rows_keeps_the_highlight_on_the_same_session() {
        let mut p = picker(
            vec![Row::new(7, "a"), Row::new(8, "b"), Row::new(9, "c")],
            2,
        );
        assert_eq!(p.chosen().unwrap().id, 9);
        p.replace(vec![Row::new(8, "b"), Row::new(9, "c")], 0);
        assert_eq!(p.chosen().unwrap().id, 9);
    }

    #[test]
    fn a_highlight_whose_row_is_gone_lands_on_the_first_one_left() {
        let mut p = picker(vec![Row::new(7, "a"), Row::new(8, "b")], 1);
        p.replace(vec![Row::new(7, "a")], 0);
        assert_eq!(p.chosen().unwrap().id, 7);
    }

    /// The box a slow fleet opens is empty, so the first real listing lands
    /// under a popup with nothing highlighted. It has to land on the session
    /// this client is attached to, which is the row the caller names and the
    /// same one the box opens on when a listing was ready in time. Falling
    /// back to the first row put Enter, the obvious next key, into somebody
    /// else's session.
    #[test]
    fn a_listing_landing_under_an_empty_popup_lands_on_the_session_you_are_in() {
        let mut p = picker(Vec::new(), 0);
        assert!(p.chosen().is_none());
        p.replace(
            vec![Row::new(0, "api"), Row::new(1, "build"), Row::new(2, "web")],
            1,
        );
        assert_eq!(p.chosen().unwrap().label, "build");
    }

    /// And back to a list you left, on the row the gesture started from: `m`
    /// then Esc. By id, because a listing may have landed in between.
    #[test]
    fn a_list_can_be_pointed_back_at_the_row_a_gesture_started_from() {
        let mut p = picker(vec![Row::new(7, "a"), Row::new(8, "b")], 0);
        p.point_at(8);
        assert_eq!(p.chosen().unwrap().id, 8);
        p.point_at(99);
        assert_eq!(
            p.chosen().unwrap().id,
            8,
            "a row that has gone moves nothing"
        );
    }

    #[test]
    fn a_list_longer_than_the_screen_scrolls_and_keeps_the_highlight_visible() {
        let rows: Vec<Row> = (0..40).map(|i| Row::new(i, format!("s{i}"))).collect();
        let mut p = picker(rows, 0);
        for _ in 0..30 {
            p.down();
        }
        let lines = seen(&mut p, BIG);
        assert!(
            lines.iter().any(|line| line.contains("s30")),
            "the highlighted row has to be on screen: {lines:#?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("s0 ")),
            "and the top of the list is not: {lines:#?}"
        );
        assert!(lines.len() <= usize::from(BIG.rows), "{lines:#?}");
    }

    #[test]
    fn a_label_wider_than_the_box_is_truncated_rather_than_wrapping_it() {
        let long = "a".repeat(200);
        let mut p = picker(vec![Row::new(0, long)], 0);
        for line in seen(&mut p, BIG) {
            assert!(
                columns(&line) <= BIG.cols,
                "{} columns: {line:?}",
                columns(&line)
            );
        }
    }

    #[test]
    fn a_title_wider_than_the_box_is_truncated_too() {
        let mut p = Picker::new("m".repeat(200), "⏎ go", vec![Row::new(0, "a")], 0);
        for line in seen(&mut p, BIG) {
            assert!(columns(&line) <= BIG.cols, "{line:?}");
        }
    }

    /// A window too small for a box that says anything gets no box, rather than
    /// a frame with the rows squeezed out of it.
    #[test]
    fn a_window_too_small_for_the_box_gets_nothing() {
        let mut p = picker(vec![Row::new(0, "a")], 0);
        assert!(p.draw(Size { cols: 20, rows: 24 }).is_empty());
        assert!(p.draw(Size { cols: 80, rows: 4 }).is_empty());
    }

    /// The tree is drawn by the indent and by nothing else, so the indent has
    /// to survive the columns: a section's sessions sit in from the section,
    /// and the two kinds of section sit level with each other.
    #[test]
    fn the_tree_is_drawn_a_step_in_per_level() {
        let rows = vec![
            Row::heading("@pi"),
            Row::heading("web-box").indent(1),
            Row::new(0, "build").indent(2),
            Row::heading("gpu-box"),
            Row::new(1, "spare").indent(1),
        ];
        let lines = seen(&mut picker(rows, 4), BIG);
        let column = |needle: &str| {
            lines
                .iter()
                .find(|line| line.contains(needle))
                .map(|line| line.find(needle).unwrap())
                .unwrap_or_else(|| panic!("{needle} was not drawn: {lines:#?}"))
        };
        assert!(column("@pi") < column("web-box"), "{lines:#?}");
        assert!(column("web-box") < column("build"), "{lines:#?}");
        // A group and a machine with nothing grouped on it are siblings: the
        // group is not inside the machine, nor the machine inside the group.
        assert_eq!(column("@pi"), column("gpu-box"), "{lines:#?}");
        assert_eq!(column("web-box"), column("spare"), "{lines:#?}");
    }

    /// The cells under the box are the box's: the session's screen there was
    /// painted over when it went up. So a box that shrinks has to clear the
    /// rows it gives up, or the first real listing landing under an open popup
    /// leaves the old box's top sitting on the screen.
    #[test]
    fn a_box_that_changes_shape_clears_what_it_no_longer_covers() {
        let tall: Vec<Row> = (0..10).map(|i| Row::new(i, format!("s{i}"))).collect();
        let mut p = picker(tall, 0);
        let before = seen(&mut p, BIG).len();
        p.replace(vec![Row::new(0, "s0")], 0);
        let after = p.draw(BIG);
        assert!(
            seen(&mut p, BIG).len() < before,
            "the box should have shrunk"
        );
        let blanked = after.matches("\x1b[K").count() + after.matches("    ").count();
        assert!(blanked > 0, "nothing was cleared: {after:?}");
        // And the rows it gave up are written over with blanks, so what was on
        // them is gone rather than left framed by nothing.
        let lines = seen(&mut p, BIG);
        assert!(
            !lines.iter().any(|line| line.contains("s9")),
            "a row from the taller box survived: {lines:#?}"
        );
    }

    /// The detail and the note are read down the box as columns, so a deeper
    /// row must not push them along: the indent is spent out of the name's own
    /// column.
    #[test]
    fn a_deeper_row_keeps_the_columns_beside_it_in_line() {
        let rows = vec![
            Row::new(0, "shallow").detail("zsh").note("1m"),
            Row::new(1, "deep").detail("zsh").note("2m").indent(2),
        ];
        let lines = seen(&mut picker(rows, 0), BIG);
        let ends: Vec<usize> = lines
            .iter()
            .filter(|line| line.contains("zsh"))
            .map(|line| line.find("zsh").unwrap())
            .collect();
        assert_eq!(ends.len(), 2, "{lines:#?}");
        assert_eq!(ends[0], ends[1], "the detail column moved: {lines:#?}");
    }

    /// `h` steps to the next run of sessions, whatever gathered it: with the
    /// groups leading the list, a key that only walked machines would step over
    /// most of what is on the screen.
    #[test]
    fn the_section_key_walks_groups_and_machines_alike() {
        let rows = vec![
            Row::heading("@pi"),
            Row::new(0, "box/a").indent(1),
            Row::heading("@web"),
            Row::new(1, "box/b").indent(1),
            Row::heading("gpu"),
            Row::new(2, "c").indent(1),
        ];
        let mut p = picker(rows, 1);
        p.next_heading(true);
        assert_eq!(p.chosen().unwrap().id, 1, "the next section");
        p.next_heading(true);
        assert_eq!(p.chosen().unwrap().id, 2, "and the machine after it");
        p.next_heading(true);
        assert_eq!(p.chosen().unwrap().id, 0, "around to the first");
    }

    /// Going back lands on the first session of the section rather than the
    /// last one above it, which is what makes the key a way of walking sections
    /// rather than a way of walking backwards one row.
    #[test]
    fn stepping_back_a_section_lands_on_its_first_session() {
        let rows = vec![
            Row::heading("@pi"),
            Row::new(0, "box/a").indent(1),
            Row::new(1, "box/b").indent(1),
            Row::heading("gpu"),
            Row::new(2, "c").indent(1),
        ];
        let mut p = picker(rows, 4);
        p.next_heading(false);
        assert_eq!(p.chosen().unwrap().id, 0, "the top of pi's run");
    }

    #[test]
    fn every_row_is_drawn_the_same_width() {
        let rows = vec![
            Row::heading("web-box"),
            Row::new(0, "build").detail("cargo test").note("2m"),
            Row::new(1, "a-much-longer-session-name").detail("nvim"),
        ];
        let mut p = picker(rows, 1);
        let lines = seen(&mut p, BIG);
        let widths: Vec<u16> = lines.iter().map(|line| columns(line)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged box: {widths:?} from {lines:#?}"
        );
    }
}
