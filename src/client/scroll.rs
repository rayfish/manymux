//! Looking back through what a session printed, on a screen the terminal keeps
//! no scrollback for.
//!
//! Only `--screen alternate` needs this. Inline the terminal has the lines in
//! its own buffer and its own wheel scrolls them, which is better than anything
//! here; this is what the other mode has instead.
//!
//! Moving a window over the node's ten thousand lines, which is arithmetic and
//! string building, and is all here so it can be tested without a terminal.
//!
//! It selects and copies as well, which it was written not to do. The reasoning
//! against was that the terminal's own selection works on whatever the view is
//! showing, so a copy mode would be a second answer to a question already
//! answered. That holds only while the terminal still has the mouse, and with
//! `mouse = client` it does not: reporting the wheel to us is the same request
//! that stops a bare drag selecting. So taking the mouse and doing nothing with
//! the drag left the gesture reaching nobody, which is the state the wheel was
//! in before it was read here. Selection is the client's wherever the mouse is,
//! and the terminal's wherever it is not.
//!
//! It stays a long way from tmux's copy mode all the same. There is no keyboard
//! selection, no marks and no registers: what is here is what a hand with a
//! mouse does, because a hand with a keyboard has the search, and the one thing
//! it produces goes to the system clipboard rather than to somewhere only
//! manymux can paste from.
//!
//! The window moves locally inside a block that has already arrived, and a
//! block is a few screenfuls. A request per wheel notch would be a round trip
//! over ssh per wheel notch, on machines that are usually two hops away.

use crate::client::attach::Spot;
use crate::client::status::session_size;
use crate::proto::{Found, Size, View as Window, ViewRequest};

/// Screenfuls fetched at a time. Enough that a page up or down lands inside
/// what is already here, so the common gesture never waits on the network.
const BLOCK: u64 = 3;

/// Lines a wheel notch moves. What a terminal sends per notch is one report,
/// and three lines is what everything else moves for one.
pub const WHEEL: u64 = 3;

/// A window over the session's history, and the block of it we hold.
pub struct Scrollback {
    /// Lines back from the newest line that the bottom of the window sits at.
    /// Zero is the live screen's last line, which is where the view opens.
    offset: u64,
    /// Lines in the whole buffer, screen included, as the host last said.
    ///
    /// Meaningless until [`Self::answered`], and zero until then, which is why
    /// that flag exists rather than a `None` here: every use of this is
    /// arithmetic, and an `Option` in the middle of it would be unwrapped to
    /// zero at each one and mean nothing.
    total: u64,
    /// Whether the host has answered once, and so whether `total` is worth
    /// clamping against.
    ///
    /// The view opens before anything has been asked for, so the first move
    /// back is made against a total of zero. Clamped against that it is thrown
    /// away, which nobody noticed while the view was opened by a key that only
    /// opened it: the first thing that moved was the second thing you pressed.
    /// The wheel opens it and moves it in one gesture, so the lost move is the
    /// first notch, and a wheel whose first notch does nothing reads as a wheel
    /// that does not work.
    answered: bool,
    /// What we have: where its bottom sits, and its lines, oldest first.
    block: Option<Block>,
    /// The last thing asked for, so a window the host cannot fill any better
    /// is not asked for again and again. Its answer arriving does not clear
    /// this: the answer is the best there is.
    asked: Option<ViewRequest>,
    /// Rows the session's part of the screen has, which is what a page is.
    rows: u16,
    /// The last search: what was looked for, where it was found, and which of
    /// those the view is sitting on.
    search: Option<Search>,
    /// What is selected, while anything is.
    selection: Option<Selection>,
    /// A copy asked for before the lines to make it out of had arrived.
    ///
    /// A hand moving quickly presses, drags and lets go inside one read, which
    /// is one pass through the client and no time at all for the block the
    /// press asked for to come back. The cells are the screen's and are known
    /// throughout; what was written on them is on the machine at the other end.
    /// So the copy waits for the answer already on its way, rather than being
    /// answered out of an empty block, which put one blank line per line
    /// dragged over on the clipboard and said `copied 3 lines` while doing it.
    owed: bool,
    /// What each row was last painted with, so a frame writes only the rows
    /// that changed.
    ///
    /// A drag reports a cell at a time and every report moves the highlight by
    /// a row or less, so repainting the window per report wrote a screenful to
    /// say a hundred bytes: a short drag came to fifty kilobytes, and on a
    /// window twice the size it is four times that. Nothing else paints over
    /// the view while it is up, which is what makes remembering safe: session
    /// output is held while a surface of the client's owns the screen, and the
    /// mark row is not one of these rows.
    ///
    /// A row nothing is known about is `None` rather than an empty string, and
    /// the two are not the same thing: the first frame of a view opened over a
    /// short buffer has blank rows at the top, and those have to be written to
    /// erase the session showing through underneath them.
    painted: Vec<Option<String>>,
}

/// A selection, in the buffer's own coordinates rather than the screen's, so
/// that scrolling under it moves what is highlighted rather than what is
/// selected. A drag that runs off the top of the window is still a drag.
#[derive(Debug, Clone, Copy)]
struct Selection {
    /// Where the button went down, which stays put.
    anchor: Cell,
    /// Where it is now, which is what the drag moves.
    head: Cell,
    /// Whether somebody asked for this outright, by clicking twice or three
    /// times, rather than dragging it out. It decides what a release does with
    /// a selection one cell wide: dragged, that is a click that never moved.
    whole: bool,
}

/// One cell of the buffer: a line counted back from the newest the way
/// [`Scrollback::offset`] is, and a column counted from zero.
///
/// Lines count backwards and columns forwards, so the pair does not order
/// itself the way reading does. [`Selection::ends`] is the one place that is
/// worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cell {
    line: u64,
    col: u16,
}

impl Selection {
    /// The two ends in reading order: where the selected text starts, and where
    /// it ends. A drag upwards selects the same cells as the same drag
    /// downwards, which is what every selection anywhere does.
    fn ends(&self) -> (Cell, Cell) {
        let (a, b) = (self.anchor, self.head);
        // A greater line offset is further back, so it reads first.
        if a.line > b.line || (a.line == b.line && a.col <= b.col) {
            (a, b)
        } else {
            (b, a)
        }
    }
}

struct Search {
    needle: String,
    /// Offsets of the matching lines, nearest the bottom first, as the host
    /// found them.
    lines: Vec<u64>,
    /// Which of them the view is on, once it has landed on one.
    at: Option<usize>,
}

struct Block {
    /// The `from` its request came back with, so the same arithmetic works on
    /// it as on the window.
    from: u64,
    lines: Vec<String>,
}

impl Block {
    /// How far back from the newest line this block reaches.
    fn top(&self) -> u64 {
        self.from + self.lines.len() as u64
    }

    /// The `rows` lines ending `offset` back from the newest, oldest first.
    /// Fewer than `rows` of them at the top of the buffer, where there is
    /// nothing older left to show.
    fn window(&self, offset: u64, rows: u64) -> &[String] {
        // Lines are stored oldest first, and offsets count back from the
        // newest, so the two run in opposite directions. Saturating rather
        // than trusting the caller: this is the one place in here where two
        // counts running opposite ways meet, and getting it wrong is a panic
        // in a debug build and a slice index out of a wrapped subtraction in a
        // release one, neither of which anybody could read afterwards.
        let back = offset.saturating_sub(self.from);
        let end = (self.lines.len() as u64).saturating_sub(back);
        let start = end.saturating_sub(rows);
        &self.lines[start as usize..end as usize]
    }
}

impl Scrollback {
    /// Open at the bottom, where the live screen is.
    pub fn new(size: Size) -> Self {
        Self {
            offset: 0,
            total: 0,
            answered: false,
            block: None,
            asked: None,
            rows: session_size(size).rows,
            search: None,
            selection: None,
            owed: false,
            painted: Vec::new(),
        }
    }

    /// Take what a search found, and go to the first match above where the view
    /// is sitting. Returns whether there was one to go to.
    ///
    /// "Above" because a search back through a history is the only direction
    /// there is anything to find in: everything below the view is on the screen
    /// already.
    pub fn found(&mut self, found: Found) -> bool {
        let mut search = Search {
            needle: found.needle,
            lines: found.lines,
            at: None,
        };
        let first = search.lines.iter().position(|line| *line > self.offset);
        search.at = first;
        self.search = Some(search);
        match first {
            Some(at) => {
                self.jump(at);
                true
            }
            None => false,
        }
    }

    /// The next match further back, or the previous one on the way down.
    /// Returns whether there was one; at either end the view stays put, which
    /// is what says there is nothing more that way.
    pub fn step(&mut self, back: bool) -> bool {
        let Some(search) = &self.search else {
            return false;
        };
        let next = match (search.at, back) {
            (Some(at), true) => at + 1,
            (Some(0), false) => return false,
            (Some(at), false) => at - 1,
            // A search that found nothing to land on, stepped through anyway.
            (None, _) => return false,
        };
        if next >= search.lines.len() {
            return false;
        }
        self.jump(next);
        true
    }

    /// Put the match in the middle of the screen, where the lines either side
    /// of it are the reason you were looking for it.
    fn jump(&mut self, at: usize) {
        let Some(search) = &mut self.search else {
            return;
        };
        let Some(line) = search.lines.get(at).copied() else {
            return;
        };
        search.at = Some(at);
        let middle = self.page() / 2;
        let furthest = self.total.saturating_sub(self.page());
        self.offset = line.saturating_sub(middle).min(furthest);
    }

    /// What the row says about the search: the needle, and which match of how
    /// many the view is on.
    pub fn searching(&self) -> Option<String> {
        let search = self.search.as_ref()?;
        let count = search.lines.len();
        Some(match search.at {
            Some(at) => format!("/{}  {}/{count}", search.needle, at + 1),
            None => format!("/{}  no matches", search.needle),
        })
    }

    pub fn resize(&mut self, size: Size) {
        self.rows = session_size(size).rows;
        // Every row is somewhere else now, so what was painted where says
        // nothing about what is there.
        self.painted.clear();
    }

    /// How far back the view is, for the row at the bottom of the screen.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Whether the view is back at the live screen, which is where leaving it
    /// costs nothing.
    pub fn at_bottom(&self) -> bool {
        self.offset == 0
    }

    fn page(&self) -> u64 {
        u64::from(self.rows).max(1)
    }

    /// Whether the block in hand holds every line the window shows.
    ///
    /// The window is a screenful unless the buffer runs out first, which is
    /// why this needs the total and the block cannot answer it alone: at the
    /// top of a short history a screenful of rows is three lines and twenty of
    /// nothing, and waiting for lines that do not exist would leave the view
    /// blank forever.
    fn covered(&self) -> bool {
        let Some(block) = &self.block else {
            return false;
        };
        let top = (self.offset + self.page()).min(self.total);
        self.offset >= block.from && top <= block.top()
    }

    /// Move back through the history. Clamped at the top of what the host has:
    /// the window's top edge cannot go past the oldest line, or a screenful of
    /// blank would sit above lines that do exist.
    pub fn up(&mut self, lines: u64) {
        // Nothing to clamp against yet, so the move stands and the request
        // built from it asks for the block around where it landed. `take` does
        // the clamping when the answer says how much history there is, which
        // is the same clamp a move made later gets and one round trip earlier
        // than owing it to a second one.
        if !self.answered {
            self.offset += lines;
            return;
        }
        let furthest = self.total.saturating_sub(self.page());
        self.offset = (self.offset + lines).min(furthest);
    }

    pub fn down(&mut self, lines: u64) {
        self.offset = self.offset.saturating_sub(lines);
    }

    pub fn page_up(&mut self) {
        self.up(self.page());
    }

    pub fn page_down(&mut self) {
        self.down(self.page());
    }

    /// The oldest line the host still has.
    ///
    /// As far back as there is, which before the first answer is as far back as
    /// there could be: `take` brings it to whatever turned out to exist.
    pub fn top(&mut self) {
        if !self.answered {
            self.offset = u64::MAX;
            return;
        }
        self.offset = self.total.saturating_sub(self.page());
    }

    /// Back to the live screen.
    pub fn bottom(&mut self) {
        self.offset = 0;
    }

    /// What to ask the host for, if what we hold does not cover the window.
    ///
    /// A block is centred on the window rather than starting at it, so moving
    /// either way stays inside it for a while. The first call always asks,
    /// because nothing has arrived yet and the total is not even known.
    pub fn wanted(&mut self) -> Option<ViewRequest> {
        if self.covered() {
            return None;
        }
        let span = self.page() * BLOCK;
        // Half a block below the window, so scrolling back down is local too,
        // and the rest above it, which is the way it is about to go.
        let below = (span - self.page()) / 2;
        let request = ViewRequest {
            from: self.offset.saturating_sub(below),
            lines: u32::try_from(span).unwrap_or(u32::MAX),
        };
        // The same request twice means the answer to the first one was all
        // there is, and asking again would only get it again, forever.
        if self.asked.as_ref() == Some(&request) {
            return None;
        }
        self.asked = Some(request.clone());
        Some(request)
    }

    /// Take a block the host sent, and learn from it where the ends are.
    pub fn take(&mut self, window: Window) {
        self.total = window.total;
        self.answered = true;
        // A buffer that trimmed under the request, or one shorter than a
        // screen: keep the window inside what exists.
        let furthest = self.total.saturating_sub(self.page());
        self.offset = self.offset.min(furthest);
        self.block = Some(Block {
            from: window.from,
            lines: window.lines,
        });
    }

    /// Where a cell of the screen sits in the buffer.
    ///
    /// The bottom row of the window is wherever the view is, and the rows above
    /// it are the lines behind it, so this is the one place the screen's
    /// coordinates and the buffer's meet. Both are the terminal's: a row and a
    /// column counting from one.
    fn cell(&self, spot: Spot) -> Cell {
        let rows = self.page();
        let row = u64::from(spot.row).clamp(1, rows);
        Cell {
            line: self.offset + (rows - row),
            col: spot.col.saturating_sub(1),
        }
    }

    /// Start a selection where the button went down, dropping whatever was
    /// selected before.
    pub fn select_from(&mut self, spot: Spot) {
        let at = self.cell(spot);
        self.selection = Some(Selection {
            anchor: at,
            head: at,
            whole: false,
        });
    }

    /// Drag it to here. Nothing at all if no button ever went down, which is a
    /// report arriving out of order rather than a gesture.
    pub fn select_to(&mut self, spot: Spot) {
        let at = self.cell(spot);
        if let Some(selection) = &mut self.selection {
            selection.head = at;
        }
    }

    /// Take the word under a second click, in the sense a terminal means:
    /// letters and digits, and the punctuation that holds a path or a URL
    /// together, so that double-clicking `src/client/scroll.rs` takes all of
    /// it rather than three letters of it.
    pub fn select_word(&mut self, spot: Spot) {
        let at = self.cell(spot);
        let Some(line) = self.line(at.line) else {
            return self.select_from(spot);
        };
        let text: Vec<char> = plain(line, 0, u16::MAX).chars().collect();
        let col = usize::from(at.col);
        if text.get(col).is_none_or(|c| !in_a_word(*c)) {
            return self.select_from(spot);
        }
        let start = text[..col]
            .iter()
            .rposition(|c| !in_a_word(*c))
            .map_or(0, |before| before + 1);
        let end = text[col..]
            .iter()
            .position(|c| !in_a_word(*c))
            .map_or(text.len(), |after| col + after)
            - 1;
        self.selection = Some(Selection {
            anchor: Cell {
                line: at.line,
                col: column(start),
            },
            head: Cell {
                line: at.line,
                col: column(end),
            },
            whole: true,
        });
    }

    /// And the whole line under a third, to the end of what is on it: the
    /// blanks past that are the screen rather than the text.
    pub fn select_line(&mut self, spot: Spot) {
        let at = self.cell(spot);
        let end = self.line(at.line).map_or(0, |line| {
            plain(line, 0, u16::MAX).trim_end().chars().count()
        });
        self.selection = Some(Selection {
            anchor: Cell {
                line: at.line,
                col: 0,
            },
            head: Cell {
                line: at.line,
                col: column(end.saturating_sub(1)),
            },
            whole: true,
        });
    }

    /// What letting go of the button has to copy, if it has anything.
    ///
    /// A click that never became a drag selects the one cell under it, and
    /// copying a character every time somebody clicks in the window is worse
    /// than doing nothing: it throws away whatever was on the clipboard, which
    /// is often the thing they were about to paste. So a one-cell selection is
    /// read as a click and dropped, unless it is a word or a line that came out
    /// one cell wide, which is a gesture somebody made twice.
    /// Whether anything is selected, which is what the highlight on the screen
    /// is showing.
    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    pub fn copied(&mut self) -> Option<String> {
        let selection = self.selection?;
        if !selection.whole && selection.anchor == selection.head {
            self.selection = None;
            return None;
        }
        if !self.holds(&selection) {
            self.owed = true;
            return None;
        }
        self.selected()
    }

    /// The copy that was waiting for its lines, now that a block has arrived.
    ///
    /// Answered once and then forgotten, whatever the block turned out to hold.
    /// The block on its way is the one the press asked for, so a block that
    /// does not cover the selection is a selection nothing later will cover
    /// either, and a request left standing would put a copy nobody asked for on
    /// the clipboard the next time the view moved.
    pub fn owed_copy(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.owed) {
            return None;
        }
        let selection = self.selection?;
        if !self.holds(&selection) {
            return None;
        }
        self.selected()
    }

    /// Whether the block in hand has the lines a selection was made on, which
    /// is what saying anything at all about its text needs.
    fn holds(&self, selection: &Selection) -> bool {
        let Some(block) = &self.block else {
            return false;
        };
        let (first, last) = selection.ends();
        // `first` is the further back of the two, so between them they are the
        // ends of the range, and everything in it is a line of the same block.
        first.line < block.top() && last.line >= block.from
    }

    /// The selected text, with the pen sequences taken out and the trailing
    /// blanks of each line with them: a line of a terminal is as wide as the
    /// screen and what was typed on it is not.
    pub fn selected(&self) -> Option<String> {
        let (first, last) = self.selection?.ends();
        let mut out = String::new();
        let mut line = first.line;
        loop {
            let text = self.line(line).unwrap_or("");
            let from = if line == first.line { first.col } else { 0 };
            let to = if line == last.line {
                last.col.saturating_add(1)
            } else {
                u16::MAX
            };
            out.push_str(plain(text, from, to).trim_end());
            if line == last.line || line == 0 {
                break;
            }
            out.push('\n');
            line -= 1;
        }
        (!out.is_empty()).then_some(out)
    }

    /// One line of the block, by how far back from the newest it is, and
    /// nothing at all for a line the block does not reach.
    ///
    /// The window arithmetic saturates at both ends, so an offset outside the
    /// block comes back with the nearest line rather than with nothing. That is
    /// what a window wants, where the clamp is the view arriving at the top of
    /// the buffer, and the opposite of what a selection wants, where it is a
    /// line nobody pointed at being copied.
    fn line(&self, offset: u64) -> Option<&str> {
        let block = self.block.as_ref()?;
        if offset < block.from || offset >= block.top() {
            return None;
        }
        block.window(offset, 1).first().map(String::as_str)
    }

    /// Which columns of the line at `offset` are selected, if any of them are.
    fn selected_on(&self, offset: u64) -> Option<(u16, u16)> {
        let (first, last) = self.selection?.ends();
        if offset > first.line || offset < last.line {
            return None;
        }
        let from = if offset == first.line { first.col } else { 0 };
        let to = if offset == last.line {
            last.col.saturating_add(1)
        } else {
            u16::MAX
        };
        (from < to).then_some((from, to))
    }

    /// The screen as it should now look: the rows of the session's part of it
    /// that are not already showing what they should, each clearing what was on
    /// it as it is written.
    ///
    /// Row by row rather than an erase and a repaint. The erase left the screen
    /// blank for as long as it took the lines after it to be drawn, which at
    /// one notch is nothing and at a wheel being spun is a flicker on every
    /// frame. Writing each row over the one before it never shows an empty
    /// screen, and clearing to the end of each line is what keeps a long line
    /// from showing through under a shorter one that replaced it.
    ///
    /// And only the rows that changed, which is what makes a drag smooth: a
    /// pointer moving reports every cell it crosses, and each report moves the
    /// highlight by a row or less. Writing the window per report is a screenful
    /// of bytes to say a hundred, and enough of them arrive per second that the
    /// terminal is doing the painting rather than the hand doing the dragging.
    ///
    /// Nothing at all while the first block is on its way, rather than a blank
    /// screen: what is up is the session, one frame more of it is no lie, and
    /// blanking the screen to wait is the flicker again with nothing to show
    /// for it.
    pub fn paint(&mut self) -> String {
        let Some(wanted) = self.rows_now() else {
            return String::new();
        };
        if self.painted.len() != wanted.len() {
            self.painted = vec![None; wanted.len()];
        }
        let mut out = String::new();
        for (i, line) in wanted.into_iter().enumerate() {
            if self.painted[i].as_deref() == Some(line.as_str()) {
                continue;
            }
            // The pen goes back to default per row: the line about to be
            // written sets its own colours, and the clear that precedes it
            // would otherwise clear to whatever the line above left set.
            out.push_str(&format!("\x1b[{};1H\x1b[0m\x1b[K", i + 1));
            out.push_str(&line);
            self.painted[i] = Some(line);
        }
        out
    }

    /// What every row of the window should have on it, top row first, with the
    /// highlight already drawn in. Nothing while there is no block to draw it
    /// from.
    fn rows_now(&self) -> Option<Vec<String>> {
        let block = self.block.as_ref()?;
        if !self.covered() {
            return None;
        }
        let rows = self.page();
        let lines = block.window(self.offset, rows);
        // A window at the very top of a short buffer has fewer lines than the
        // screen has rows, and they go at the bottom of it: the oldest line
        // sits where it would have been had you scrolled to it.
        let blank = rows.saturating_sub(lines.len() as u64);
        Some(
            (1..=rows)
                .map(|row| {
                    if row <= blank {
                        return String::new();
                    }
                    let line = &lines[(row - blank - 1) as usize];
                    match self.selected_on(self.offset + (rows - row)) {
                        Some((from, to)) => highlighted(line, from, to),
                        None => line.clone(),
                    }
                })
                .collect(),
        )
    }
}

/// Whether a character is part of what a double click takes.
///
/// Wider than a word, because what people double click in a terminal is a path,
/// a flag or a URL as often as it is a word, and taking three letters out of
/// the middle of one is a gesture that has to be repaired by hand.
fn in_a_word(c: char) -> bool {
    c.is_alphanumeric() || "_-./+@:~=".contains(c)
}

fn column(at: usize) -> u16 {
    u16::try_from(at).unwrap_or(u16::MAX)
}

/// Walk a rendered line and hand back the text of the cells from `from` up to
/// but not including `to`.
///
/// A line arrives with the pen sequences that coloured it (`node::history`
/// writes them in), so the columns a hand pointed at are not byte offsets and
/// not character offsets either. The sequences go by without counting, which is
/// the whole of the difference.
///
/// Columns are counted in characters, as everything else in this client counts
/// them (`status::columns`): a double-width character throws the count off by
/// one, which costs a selection a cell at the end of a line of CJK and nothing
/// anywhere else.
fn plain(line: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    walk(line, |piece| {
        if let Piece::Char(c, col) = piece
            && col >= from
            && col < to
        {
            out.push(c);
        }
    });
    out
}

/// The same walk, with those cells written in reverse video.
///
/// The pen is put back rather than cleared, so a highlight over coloured text
/// leaves the colour either side of it alone: `\x1b[27m` turns reverse off and
/// says nothing about anything else.
fn highlighted(line: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    let mut inside = false;
    let end = walk(line, |piece| match piece {
        // The line's own pen, kept exactly as it came: the highlight is
        // something drawn over the colours rather than instead of them.
        Piece::Pen(pen) => out.push_str(pen),
        Piece::Char(c, col) => {
            let wanted = col >= from && col < to;
            if wanted && !inside {
                out.push_str("\x1b[7m");
            } else if !wanted && inside {
                out.push_str("\x1b[27m");
            }
            inside = wanted;
            out.push(c);
        }
    });
    // A selection that runs past the end of the text takes the blanks with it,
    // or a dragged-over empty line would show nothing at all and a multi-line
    // selection would come apart at every short line in it.
    if to > end {
        if !inside {
            out.push_str("\x1b[7m");
        }
        let blanks = usize::from(to.min(u16::MAX - 1).saturating_sub(end.max(from)));
        out.push_str(&" ".repeat(blanks.min(1000)));
        inside = true;
    }
    if inside {
        out.push_str("\x1b[27m");
    }
    out
}

/// A rendered line comes in two kinds of piece.
enum Piece<'a> {
    /// A character that takes a cell, and the column it takes.
    Char(char, u16),
    /// A sequence that takes none: the pen the line was written with.
    Pen(&'a str),
}

/// Hand a line to `each` a piece at a time, and answer with the column it ends
/// at.
///
/// The reason this exists at all is that a line arrives coloured, so a column
/// is neither a byte offset into it nor a character offset: the pen sequences
/// sit between the cells and take none. One walk answers both callers because
/// they differ only in what they do with the pieces.
fn walk(line: &str, mut each: impl FnMut(Piece<'_>)) -> u16 {
    let mut col = 0u16;
    let mut chars = line.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        if c != '\x1b' {
            each(Piece::Char(c, col));
            col = col.saturating_add(1);
            continue;
        }
        // A control sequence runs to a byte in `@` to `~` and an OSC to a bell
        // or a backslash. Anything else an escape starts is two characters.
        let mut end = at + c.len_utf8();
        match chars.peek().map(|(_, c)| *c) {
            Some('[') => {
                chars.next();
                end += 1;
                for (at, c) in chars.by_ref() {
                    end = at + c.len_utf8();
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                end += 1;
                for (at, c) in chars.by_ref() {
                    end = at + c.len_utf8();
                    if c == '\x07' || c == '\\' {
                        break;
                    }
                }
            }
            Some(next) => {
                chars.next();
                end += next.len_utf8();
            }
            None => {}
        }
        each(Piece::Pen(&line[at..end]));
    }
    col
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size { cols: 80, rows: 25 };

    /// The host, standing in: answers whatever the view asks for out of a
    /// buffer of numbered lines, with the clamping `node::history::window`
    /// does. Answering by hand instead would let the tests agree with
    /// themselves about arithmetic the node does differently.
    fn answer(view: &mut Scrollback, total: u64) {
        let Some(request) = view.wanted() else {
            return;
        };
        let from = request.from.min(total.saturating_sub(1));
        let bottom = total - from;
        let top = bottom.saturating_sub(u64::from(request.lines));
        view.take(Window {
            from,
            total,
            lines: (top..bottom).map(|i| format!("line {i}")).collect(),
        });
    }

    /// A screen of 25 rows keeps one for the mark, so a page is 24.
    fn view(total: u64) -> Scrollback {
        let mut view = Scrollback::new(SIZE);
        answer(&mut view, total);
        view
    }

    #[test]
    fn a_view_opens_at_the_live_screen() {
        let view = Scrollback::new(SIZE);
        assert_eq!(view.offset(), 0);
        assert!(view.at_bottom());
    }

    /// The bug the wheel found: the view opens on the first notch, so the move
    /// that opened it is made before the host has said how much history there
    /// is. Clamped against a total of zero it was thrown away, and the first
    /// notch of the gesture did nothing at all.
    #[test]
    fn the_move_that_opened_the_view_is_not_lost_to_a_total_nobody_knows_yet() {
        let mut view = Scrollback::new(SIZE);
        view.up(WHEEL);
        answer(&mut view, 100);
        assert_eq!(view.offset(), 3, "the notch that opened the view moved it");
    }

    /// And it is still a move, so what exists still bounds it: a hand that
    /// spun the wheel into a session with barely any history lands on the
    /// oldest line rather than past it.
    #[test]
    fn an_opening_move_past_the_end_is_brought_back_when_the_answer_lands() {
        let mut view = Scrollback::new(SIZE);
        view.up(10_000);
        answer(&mut view, 100);
        assert_eq!(view.offset(), 100 - 24);

        // `g` before the first answer means the same thing, and cannot say so
        // in lines because it does not yet know how many there are.
        let mut view = Scrollback::new(SIZE);
        view.top();
        answer(&mut view, 100);
        assert_eq!(view.offset(), 100 - 24);
    }

    #[test]
    fn scrolling_back_stops_at_the_oldest_line_there_is() {
        let mut view = view(100);
        view.up(WHEEL);
        assert_eq!(view.offset(), 3);
        view.page_up();
        assert_eq!(view.offset(), 27);
        view.up(1000);
        assert_eq!(
            view.offset(),
            100 - 24,
            "the window's top edge sits on the oldest line, not past it"
        );
        view.top();
        assert_eq!(view.offset(), 76);
    }

    #[test]
    fn scrolling_forward_stops_at_the_live_screen() {
        let mut view = view(100);
        view.top();
        view.page_down();
        assert_eq!(view.offset(), 52);
        view.down(1000);
        assert_eq!(view.offset(), 0);
        assert!(view.at_bottom());
    }

    /// A session that has printed less than a screenful has nowhere to go.
    #[test]
    fn a_buffer_shorter_than_the_screen_does_not_move() {
        let mut view = view(5);
        view.page_up();
        assert_eq!(view.offset(), 0);
        view.top();
        assert_eq!(view.offset(), 0);
    }

    #[test]
    fn the_first_request_asks_for_a_block_around_the_bottom() {
        let mut view = Scrollback::new(SIZE);
        let wanted = view.wanted().expect("nothing has arrived yet");
        assert_eq!(wanted.from, 0, "there is nothing below the live screen");
        assert_eq!(wanted.lines, 72, "three screenfuls");
    }

    /// The point of a block: moving inside what has already arrived asks for
    /// nothing, so a wheel notch is not a round trip over ssh.
    #[test]
    fn moving_inside_the_block_asks_for_nothing() {
        let mut view = view(1000);
        assert!(view.wanted().is_none());
        view.up(WHEEL);
        assert!(view.wanted().is_none());
        view.page_up();
        assert!(
            view.wanted().is_none(),
            "a page up is still inside a three page block"
        );
    }

    #[test]
    fn leaving_the_block_asks_for_another_around_where_you_are() {
        let mut view = view(1000);
        for _ in 0..3 {
            view.page_up();
        }
        let wanted = view.wanted().expect("the window has left the block");
        assert_eq!(wanted.lines, 72);
        assert_eq!(wanted.from, 72 - 24, "half a block below the window");
    }

    /// A view at the top of what the host has keeps asking for lines that are
    /// not there otherwise, once per keystroke, forever.
    #[test]
    fn the_same_window_is_not_asked_for_twice() {
        let mut view = view(30);
        view.top();
        answer(&mut view, 30);
        assert!(view.wanted().is_none());
        view.up(WHEEL);
        assert!(view.wanted().is_none(), "there is nowhere further to go");
    }

    /// The host is the one that knows how much history there is, and it says so
    /// with every window. A view sitting past a top that has trimmed away comes
    /// back to what still exists.
    #[test]
    fn a_buffer_that_trimmed_under_the_view_pulls_it_back() {
        let mut view = view(1000);
        view.top();
        assert_eq!(view.offset(), 976);

        view.take(Window {
            from: 0,
            total: 100,
            lines: (0..100).map(|i| format!("line {i}")).collect(),
        });
        assert_eq!(view.offset(), 76, "the oldest line there is now");
    }

    #[test]
    fn painting_puts_the_newest_line_of_the_window_on_the_last_row() {
        let mut view = view(100);
        view.up(1);
        let painted = view.paint();
        // No screen-wide erase: each row clears itself as it is written, so
        // there is never a frame with an empty screen in it.
        assert!(!painted.contains("\x1b[2J"), "{painted:?}");
        // The window ends one line back from the newest, so its last line is
        // line 98 of 0..100, and it goes on the last row the session has.
        assert!(
            painted.contains("\x1b[24;1H\x1b[0m\x1b[Kline 98"),
            "{painted:?}"
        );
        assert!(
            painted.contains("\x1b[1;1H\x1b[0m\x1b[Kline 75"),
            "{painted:?}"
        );
        assert!(!painted.contains("line 99"), "{painted:?}");
    }

    /// At the top of a buffer shorter than the screen there is nothing to put
    /// on the first rows, and what there is belongs at the bottom.
    #[test]
    fn a_short_buffer_paints_against_the_bottom_of_the_screen() {
        let mut view = view(3);
        let painted = view.paint();
        assert!(
            painted.contains("\x1b[22;1H\x1b[0m\x1b[Kline 0"),
            "{painted:?}"
        );
        assert!(
            painted.contains("\x1b[24;1H\x1b[0m\x1b[Kline 2"),
            "{painted:?}"
        );
        // And the rows above them are cleared rather than left holding
        // whatever the last window put there, which is the job the erase this
        // no longer does used to do.
        assert!(
            painted.contains("\x1b[1;1H\x1b[0m\x1b[K\x1b["),
            "{painted:?}"
        );
    }

    /// The window is only ever asked for inside the block, but it is asked for
    /// with two counts that run in opposite directions, so it says what it does
    /// off the ends rather than leaving a subtraction to wrap.
    #[test]
    fn a_window_outside_the_block_is_empty_rather_than_a_panic() {
        let block = Block {
            from: 10,
            lines: vec!["a".to_string(), "b".to_string()],
        };
        assert_eq!(block.window(0, 24).len(), 2, "below the block");
        assert!(block.window(1000, 24).is_empty(), "above it");
        assert_eq!(block.window(10, 1), ["b"], "the newest line it holds");
    }

    fn found(lines: Vec<u64>) -> Found {
        Found {
            needle: "boom".to_string(),
            lines,
        }
    }

    /// A search goes back through the history, because everything below the
    /// view is on the screen already, and lands with the match in the middle so
    /// the lines either side of it are there too.
    #[test]
    fn a_search_lands_on_the_first_match_above_the_view() {
        let mut view = view(1000);
        assert!(view.found(found(vec![100, 400, 900])));
        assert_eq!(view.offset(), 100 - 12, "the match, half a screen up");
        assert_eq!(view.searching().as_deref(), Some("/boom  1/3"));
    }

    #[test]
    fn n_walks_back_through_the_matches_and_shift_n_comes_back() {
        let mut view = view(1000);
        view.found(found(vec![100, 400, 900]));

        assert!(view.step(true));
        assert_eq!(view.offset(), 400 - 12);
        assert_eq!(view.searching().as_deref(), Some("/boom  2/3"));

        assert!(view.step(true));
        assert_eq!(view.offset(), 900 - 12);
        assert!(!view.step(true), "there is no fourth match");
        assert_eq!(view.offset(), 900 - 12, "and the view stays where it was");

        assert!(view.step(false));
        assert_eq!(view.offset(), 400 - 12);
        assert!(view.step(false));
        assert!(!view.step(false), "nor a match below the first");
    }

    /// A match near the top cannot be centred: there is nothing older to put
    /// above it, and the view stops where the history does.
    #[test]
    fn a_match_at_the_top_is_shown_against_the_top() {
        let mut view = view(100);
        view.found(found(vec![99]));
        assert_eq!(view.offset(), 100 - 24);
    }

    #[test]
    fn a_search_that_finds_nothing_says_so_and_stays_put() {
        let mut view = view(1000);
        view.page_up();
        let before = view.offset();
        assert!(!view.found(found(Vec::new())));
        assert_eq!(view.offset(), before);
        assert_eq!(view.searching().as_deref(), Some("/boom  no matches"));
        assert!(!view.step(true));
    }

    /// Until the first block lands there is nothing to draw, so nothing is
    /// drawn: the session is on the screen and one frame more of it is no lie,
    /// where blanking the screen to wait is a flicker with nothing to show for
    /// it. The block is a round trip away and paints over it when it lands.
    #[test]
    fn a_view_with_nothing_in_it_yet_leaves_the_screen_alone() {
        let mut view = Scrollback::new(SIZE);
        assert_eq!(view.paint(), "");
    }

    /// The terminal counts from one, and the bottom row of the window is
    /// wherever the view is sitting.
    fn at(row: u16, col: u16) -> Spot {
        Spot { row, col }
    }

    /// A drag along one line takes the cells between its ends, which is what
    /// every selection anywhere does.
    #[test]
    fn a_drag_takes_the_cells_it_was_dragged_over() {
        let mut view = view(100);
        // The bottom row shows line 99 of 0..100, `line 99` being 7 cells.
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        assert_eq!(view.copied().as_deref(), Some("line"));
    }

    /// Backwards is the same selection. A hand that started at the end of what
    /// it wanted did not want the other end of the line.
    #[test]
    fn a_drag_the_other_way_takes_the_same_cells() {
        let mut view = view(100);
        view.select_from(at(24, 4));
        view.select_to(at(24, 1));
        assert_eq!(view.copied().as_deref(), Some("line"));
    }

    /// Down the screen it takes whole lines between the ends, and the trailing
    /// blanks of each go: a line of a terminal is as wide as the screen, and
    /// what was typed on it is not.
    #[test]
    fn a_drag_down_the_screen_takes_whole_lines_between_its_ends() {
        let mut view = view(100);
        view.select_from(at(22, 6));
        view.select_to(at(24, 6));
        assert_eq!(view.copied().as_deref(), Some("97\nline 98\nline 9"));
    }

    /// A click is not a selection. Copying the one cell under it would replace
    /// whatever was on the clipboard, which is often the thing about to be
    /// pasted.
    #[test]
    fn a_click_that_never_moved_copies_nothing() {
        let mut view = view(100);
        view.select_from(at(24, 3));
        assert_eq!(view.copied(), None);
        assert!(!view.selecting(), "and the highlight goes with it");
    }

    /// A second click takes the word under it, and a word here is what a hand
    /// in a terminal points at: a path, a flag or a URL, not three letters out
    /// of the middle of one.
    #[test]
    fn a_second_click_takes_the_word_under_it() {
        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec!["cd src/client/scroll.rs and go".to_string()],
        });
        view.select_word(at(24, 6));
        assert_eq!(view.copied().as_deref(), Some("src/client/scroll.rs"));

        // On a blank, there is no word to take and nothing is selected.
        view.select_word(at(24, 3));
        assert_eq!(view.copied(), None);
    }

    /// And a third takes the line, to the end of what is on it.
    #[test]
    fn a_third_click_takes_the_line() {
        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec!["cd src and go".to_string()],
        });
        view.select_line(at(24, 6));
        assert_eq!(view.copied().as_deref(), Some("cd src and go"));
    }

    /// The selection is kept in the buffer's coordinates, so scrolling moves
    /// the highlight rather than what is selected. A drag that ran off the top
    /// of the window is still a drag.
    #[test]
    fn scrolling_under_a_selection_does_not_change_what_is_selected() {
        let mut view = view(100);
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        view.page_up();
        answer(&mut view, 100);
        assert_eq!(view.copied().as_deref(), Some("line"));
    }

    /// The highlight is drawn over the line's own colours rather than instead
    /// of them: what comes back off the wire is coloured text, and a copy mode
    /// that dropped the colours would repaint the screen in white on the way
    /// past.
    #[test]
    fn the_highlight_leaves_the_lines_own_colours_alone() {
        let coloured = "\x1b[31mred\x1b[0m plain";
        let painted = highlighted(coloured, 4, 9);
        assert!(painted.contains("\x1b[31m"), "{painted:?}");
        assert_eq!(plain(&painted, 0, u16::MAX), "red plain");
        // Reverse goes on at the fifth cell and off after the ninth.
        assert!(painted.contains("\x1b[7mplain\x1b[27m"), "{painted:?}");
    }

    /// The columns a hand points at are cells, and a coloured line has more
    /// bytes than cells. Counting either of the other two would take the wrong
    /// text off every line a program coloured.
    #[test]
    fn columns_are_cells_and_not_bytes() {
        let line = "\x1b[1;32mok\x1b[0m: done";
        assert_eq!(plain(line, 0, 2), "ok");
        assert_eq!(plain(line, 4, 8), "done");
        assert_eq!(plain(line, 0, u16::MAX), "ok: done");
    }

    /// A selection that runs past the end of a short line takes the blanks with
    /// it, or a block dragged down the screen would come apart at every line
    /// shorter than the one above it.
    #[test]
    fn a_highlight_past_the_end_of_a_line_covers_the_rest_of_the_row() {
        let painted = highlighted("ab", 0, 6);
        assert!(painted.starts_with("\x1b[7mab"), "{painted:?}");
        assert!(painted.contains("    \x1b[27m"), "{painted:?}");
    }

    /// A hand moving quickly presses, drags and lets go inside one read, which
    /// is one pass through the client and no time at all for the block the
    /// press asked for to arrive. Answered out of what is there, the copy is a
    /// blank line per line dragged over.
    #[test]
    fn a_copy_asked_for_before_its_lines_arrived_waits_for_them() {
        let mut view = Scrollback::new(SIZE);
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        assert_eq!(view.copied(), None, "nothing to copy out of yet");
        assert!(view.selecting(), "and the gesture is not over");

        answer(&mut view, 100);
        assert_eq!(view.owed_copy().as_deref(), Some("line"));
    }

    /// Owed once. The block that arrives is the answer to the request the press
    /// made, so one that does not cover the selection is one nothing later will
    /// cover either, and a copy still owed would land on the clipboard the next
    /// time the view moved.
    #[test]
    fn a_copy_that_was_owed_is_answered_once() {
        let mut view = Scrollback::new(SIZE);
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        view.copied();
        answer(&mut view, 100);
        assert!(view.owed_copy().is_some());
        assert_eq!(view.owed_copy(), None);
    }

    /// Nothing is owed by a copy that could be made on the spot, or letting go
    /// twice would copy twice.
    #[test]
    fn a_copy_that_could_be_made_owes_nothing() {
        let mut view = view(100);
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        assert_eq!(view.copied().as_deref(), Some("line"));
        assert_eq!(view.owed_copy(), None);
    }

    /// A drag reports every cell it crosses and each report moves the highlight
    /// by a row or less. Painting the window per report writes a screenful to
    /// say a hundred bytes, which is what a drag that stutters is made of.
    #[test]
    fn a_frame_paints_the_rows_that_changed_and_no_others() {
        let mut view = view(100);
        assert!(
            view.paint().contains("line 99"),
            "the first frame is all of it"
        );
        assert_eq!(view.paint(), "", "and the second says nothing");

        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        let painted = view.paint();
        assert!(painted.contains("\x1b[7m"), "{painted:?}");
        assert_eq!(painted.matches("\x1b[K").count(), 1, "one row: {painted:?}");
    }

    /// A row nothing has been painted to is not a row painted blank. The first
    /// frame of a view opened over a buffer shorter than the screen has blank
    /// rows at the top, and the session is showing through underneath them.
    #[test]
    fn the_blank_rows_of_a_short_buffer_are_painted_the_first_time() {
        let mut view = view(3);
        let painted = view.paint();
        assert_eq!(
            painted.matches("\x1b[K").count(),
            24,
            "every row of the window: {painted:?}"
        );
        assert_eq!(view.paint(), "");
    }
}
