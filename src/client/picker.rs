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

/// Columns a row moves in per level of the tree. Two, because the deepest row
/// is four columns in and the label column is eighteen: a step wide enough to
/// read costs the session names the room to be told apart.
const STEP: u16 = 2;

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
    /// A heading rather than something you can land on: a machine's name above
    /// its sessions, or a group's above the sessions in it. Skipped by the
    /// highlight, since Enter on one would have nothing to attach to.
    pub heading: bool,
    /// How far in the row sits, in steps of [`STEP`] columns. The list is a
    /// tree two deep, and this is the only thing that says so: a machine at
    /// nothing, a group and the sessions in no group one step in, and the
    /// sessions in a group one further.
    ///
    /// It also tells the two kinds of heading apart, which `h` needs: a machine
    /// is the only heading at nothing.
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

    /// A machine's name, which is what `h` walks between.
    pub fn heading(label: impl Into<String>) -> Self {
        Self {
            heading: true,
            ..Self::new(usize::MAX, label)
        }
    }

    /// A group's name, under the machine whose sessions are in it. A heading
    /// like the machine's, and set apart from one by sitting a step in.
    pub fn subheading(label: impl Into<String>) -> Self {
        Self::heading(label).indent(1)
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
    pub fn replace(&mut self, rows: Vec<Row>) {
        let was = self.chosen().map(|row| row.id);
        self.rows = rows;
        self.at = was
            .and_then(|id| {
                self.rows
                    .iter()
                    .position(|row| row.id == id && !row.heading)
            })
            .or_else(|| self.first())
            .unwrap_or(0);
        self.top = 0;
    }

    fn landable(&self, index: usize) -> Option<&Row> {
        self.rows.get(index).filter(|row| !row.heading)
    }

    fn first(&self) -> Option<usize> {
        self.rows.iter().position(|row| !row.heading)
    }

    /// The first row under the next machine, or the previous one.
    ///
    /// A bigger step of the same gesture: with the machines drawn as headings,
    /// `h` jumping between them is what it obviously means, and it commits
    /// nothing, which Enter is for.
    ///
    /// Machines only, never the groups under one. Those are walked with the
    /// same key every other row is, and a key that stepped over both would give
    /// a machine with three groups on it three presses where it used to have
    /// one.
    pub fn next_heading(&mut self, forwards: bool) {
        let len = self.rows.len();
        if len == 0 {
            return;
        }
        let mut seen_machine = false;
        let mut next = self.at;
        for _ in 0..len {
            next = if forwards {
                (next + 1) % len
            } else {
                (next + len - 1) % len
            };
            if self.rows[next].heading {
                seen_machine |= self.rows[next].indent == 0;
                continue;
            }
            // Going back, the row wanted is the first under the machine rather
            // than the last one above it, so keep walking to the top of the run.
            if seen_machine && (forwards || self.starts_a_machine(next)) {
                self.at = next;
                return;
            }
        }
    }

    /// Whether this row is the first that can be landed on under its machine.
    ///
    /// Which is not the same as sitting under a heading, now that a group is
    /// one too: the first session of the first group on a machine has two
    /// headings above it, and the first session of the second group has one and
    /// is in the middle of its machine's run.
    fn starts_a_machine(&self, index: usize) -> bool {
        self.rows[..index]
            .iter()
            .rev()
            .take_while(|row| row.heading)
            .any(|row| row.indent == 0)
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
    pub fn draw(&self, size: Size) -> String {
        let Some(shape) = self.shape(size) else {
            return String::new();
        };
        let mut out = String::from("\x1b7");
        let width = shape.inner;
        let mut line = shape.row;
        let mut put = |line: u16, text: &str| {
            let mut piece = at(line, shape.col);
            piece.push_str(text);
            out.push_str(&piece);
        };

        put(
            line,
            &style::faint(&format!("┌{}┐", fit(&format!(" {} ", self.title), width))),
        );
        line += 1;

        for index in shape.top..shape.top + shape.visible {
            let painted = match self.rows.get(index) {
                // A short list still gets the box it asked for, so the frame
                // does not jump as rows come and go under a listing.
                None => style::faint(&" ".repeat(usize::from(width))),
                Some(row) => self.paint(row, width, index == self.at),
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

    /// One row, exactly `width` columns wide.
    ///
    /// Every column is fixed before anything is written, because a row that
    /// sizes itself to its contents is a box with a ragged edge, and the edge is
    /// the only thing telling you where the popup stops and the session behind
    /// it starts.
    fn paint(&self, row: &Row, width: u16, here: bool) -> String {
        // The tree, and the whole of it: a row says where it sits by how far in
        // it starts, and nothing else in the box draws the shape.
        let inset = " ".repeat(usize::from(row.indent * STEP));
        if row.heading {
            let label = fit(&format!("{inset}{}", row.label), width - 2);
            return format!("  {}", style::host(&label));
        }

        // Six columns of gap, and the note pinned to the right. The detail
        // gives way first and the label is clipped last: which session it is
        // matters more than what it is doing.
        const GAPS: u16 = 6;
        const NOTE: u16 = 6;
        let label_width = 18.min(width.saturating_sub(GAPS + NOTE));
        let detail_width = width.saturating_sub(GAPS + NOTE + label_width);
        // Inside the label's column rather than in front of it, so the detail
        // and the note stay in line down the box however deep the row is.
        let label = fit(&format!("{inset}{}", row.label), label_width);
        let detail = fit(&row.detail, detail_width);
        let note = right(&row.note, NOTE);

        if here {
            // Reverse video rather than a colour, so the highlight survives a
            // terminal whose palette makes any one colour unreadable.
            format!("\x1b[7m  {label} {detail}  {note} \x1b[27m")
        } else {
            format!(
                "  {} {}  {} ",
                style::bold(&label),
                style::faint(&detail),
                style::faint(&note)
            )
        }
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
    fn seen(picker: &Picker, size: Size) -> Vec<String> {
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
        p.replace(vec![Row::new(8, "b"), Row::new(9, "c")]);
        assert_eq!(p.chosen().unwrap().id, 9);
    }

    #[test]
    fn a_highlight_whose_row_is_gone_lands_on_the_first_one_left() {
        let mut p = picker(vec![Row::new(7, "a"), Row::new(8, "b")], 1);
        p.replace(vec![Row::new(7, "a")]);
        assert_eq!(p.chosen().unwrap().id, 7);
    }

    #[test]
    fn a_list_longer_than_the_screen_scrolls_and_keeps_the_highlight_visible() {
        let rows: Vec<Row> = (0..40).map(|i| Row::new(i, format!("s{i}"))).collect();
        let mut p = picker(rows, 0);
        for _ in 0..30 {
            p.down();
        }
        let lines = seen(&p, BIG);
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
        let p = picker(vec![Row::new(0, long)], 0);
        for line in seen(&p, BIG) {
            assert!(
                columns(&line) <= BIG.cols,
                "{} columns: {line:?}",
                columns(&line)
            );
        }
    }

    #[test]
    fn a_title_wider_than_the_box_is_truncated_too() {
        let p = Picker::new("m".repeat(200), "⏎ go", vec![Row::new(0, "a")], 0);
        for line in seen(&p, BIG) {
            assert!(columns(&line) <= BIG.cols, "{line:?}");
        }
    }

    /// A window too small for a box that says anything gets no box, rather than
    /// a frame with the rows squeezed out of it.
    #[test]
    fn a_window_too_small_for_the_box_gets_nothing() {
        let p = picker(vec![Row::new(0, "a")], 0);
        assert!(p.draw(Size { cols: 20, rows: 24 }).is_empty());
        assert!(p.draw(Size { cols: 80, rows: 4 }).is_empty());
    }

    /// The tree is drawn by the indent and by nothing else, so the indent has
    /// to survive the columns: a group's sessions sit in from the group, which
    /// sits in from the machine.
    #[test]
    fn the_tree_is_drawn_a_step_in_per_level() {
        let rows = vec![
            Row::heading("dev.box.ray"),
            Row::subheading("pi"),
            Row::new(0, "build").indent(2),
            Row::new(1, "spare").indent(1),
        ];
        let lines = seen(&picker(rows, 3), BIG);
        let column = |needle: &str| {
            lines
                .iter()
                .find(|line| line.contains(needle))
                .map(|line| line.find(needle).unwrap())
                .unwrap_or_else(|| panic!("{needle} was not drawn: {lines:#?}"))
        };
        assert!(column("dev.box.ray") < column("pi"), "{lines:#?}");
        assert!(column("pi") < column("build"), "{lines:#?}");
        // And what is in no group sits with the groups rather than inside one.
        assert_eq!(column("pi"), column("spare"), "{lines:#?}");
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
        let lines = seen(&picker(rows, 0), BIG);
        let ends: Vec<usize> = lines
            .iter()
            .filter(|line| line.contains("zsh"))
            .map(|line| line.find("zsh").unwrap())
            .collect();
        assert_eq!(ends.len(), 2, "{lines:#?}");
        assert_eq!(ends[0], ends[1], "the detail column moved: {lines:#?}");
    }

    /// `h` walks machines. A group is a heading too now, and stepping over both
    /// would give a machine with three groups on it three presses where it used
    /// to have one.
    #[test]
    fn the_host_key_steps_over_the_groups_under_a_machine() {
        let rows = vec![
            Row::heading("box"),
            Row::subheading("pi"),
            Row::new(0, "a").indent(2),
            Row::subheading("web"),
            Row::new(1, "b").indent(2),
            Row::heading("gpu"),
            Row::new(2, "c").indent(1),
        ];
        let mut p = picker(rows, 0);
        p.next_heading(true);
        assert_eq!(p.chosen().unwrap().id, 2, "on to the next machine");
        p.next_heading(true);
        assert_eq!(p.chosen().unwrap().id, 0, "and around to the first");
    }

    /// Going back lands on the first session under the machine, which is no
    /// longer the row after a heading: the first group's first session has two
    /// headings above it, and the second group's has one in the middle of the
    /// run.
    #[test]
    fn stepping_back_a_machine_lands_on_its_first_session() {
        let rows = vec![
            Row::heading("box"),
            Row::subheading("pi"),
            Row::new(0, "a").indent(2),
            Row::subheading("web"),
            Row::new(1, "b").indent(2),
            Row::heading("gpu"),
            Row::new(2, "c").indent(1),
        ];
        let mut p = picker(rows, 6);
        p.next_heading(false);
        assert_eq!(p.chosen().unwrap().id, 0, "the top of box's run");
    }

    #[test]
    fn every_row_is_drawn_the_same_width() {
        let rows = vec![
            Row::heading("dev.box.ray"),
            Row::new(0, "build").detail("cargo test").note("2m"),
            Row::new(1, "a-much-longer-session-name").detail("nvim"),
        ];
        let p = picker(rows, 1);
        let lines = seen(&p, BIG);
        let widths: Vec<u16> = lines.iter().map(|line| columns(line)).collect();
        assert!(
            widths.windows(2).all(|pair| pair[0] == pair[1]),
            "ragged box: {widths:?} from {lines:#?}"
        );
    }
}
