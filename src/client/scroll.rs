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
//! What a selected cell looks like is [`crate::client::pen`]'s: a band under
//! the text rather than a repaint of it, which is a question about colour and
//! contrast rather than about scrolling.
//!
//! It stays a long way from tmux's copy mode all the same. There is no keyboard
//! selection, no marks and no registers: what is here is what a hand with a
//! mouse does, because a hand with a keyboard has the search, and the one thing
//! it produces goes to the system clipboard rather than to somewhere only
//! manymux can paste from.
//!
//! What a hand with a mouse does includes keeping hold of what it grabbed. A
//! double or a third click sets a [`Grain`] that the drag after it goes on
//! asking for, so words are dragged out by the word and lines by the line, the
//! way they are everywhere else. It is applied where the ends are read rather
//! than written into them as they move, for two reasons. The hand goes back
//! and forth over the same word, and a grain baked in on the way out could not
//! be undone on the way back. And whether there is a word there at all, and
//! where it ends, are facts about the text, which on the live screen is a
//! round trip away: the first press of a double click is what opens the view,
//! so the second one lands before the lines it is pointing at have arrived,
//! and a word worked out then is one character long.
//!
//! The window moves locally inside a block that has already arrived, and a
//! block is a few screenfuls. A request per wheel notch would be a round trip
//! over ssh per wheel notch, on machines that are usually two hops away.

use crate::client::attach::Spot;
use crate::client::pen;
use crate::proto::{Found, Size, View as Window, ViewRequest};
use unicode_width::UnicodeWidthChar;

/// Screenfuls fetched at a time. Enough that a page up or down lands inside
/// what is already here, so the common gesture never waits on the network.
const BLOCK: u64 = 3;

/// Lines a wheel notch moves. What a terminal sends per notch is one report,
/// and three lines is what everything else moves for one.
pub const WHEEL: u64 = 3;

/// Lines one copy can be made of, which is as many as one block can hold.
///
/// A selection used to be at most a screenful, because the only way to reach
/// a line was to be looking at it; a drag that scrolls the view under itself
/// has no such bound, and the text of one is built out of a single block
/// ([`Scrollback::holds`]). A block arrives in one frame, so something has to
/// say how big it may get, and it is easier to answer here than to find out
/// from the far end that it was too much. Two thousand lines is a minute and a
/// half of holding the pointer at the edge, and around a quarter of a megabyte
/// of text.
pub const COPY_LINES: u64 = 2000;

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
    /// Columns it has, which is where a highlight has to stop.
    ///
    /// A row is as wide as the screen and no wider: a highlight padded past
    /// that wraps onto the row below and takes the picture with it, since what
    /// wrapped is written before the rows under it and there is nothing to say
    /// it happened.
    cols: u16,
    /// The last search: what was looked for, where it was found, and which of
    /// those the view is sitting on.
    search: Option<Search>,
    /// What is selected, while anything is.
    selection: Option<Selection>,
    /// Where a drag is being held, while it is being held against an edge of
    /// the window, which is a drag asking for the view to move under it.
    ///
    /// The spot rather than a direction, because the view moving is what makes
    /// the row under the pointer a different line: the same spot read again
    /// against the new offset is where the drag has got to.
    chasing: Option<Spot>,
    /// Moves the chase in hand has made, which is what it speeds up with.
    chased: u64,
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
    /// How much each end takes, which is what the gesture that started this
    /// asked for.
    grain: Grain,
}

/// How much of what it lands on each end of a selection takes.
///
/// The gesture that starts a selection says this, and the drag after it keeps
/// asking for the same thing, which is what every terminal and every editor
/// does and what makes a drag feel like it is holding on to something. Set at
/// the press and applied whenever the ends are read, rather than written into
/// the ends themselves: the hand goes back and forth over a word, and a grain
/// baked into `head` on the way out could not be undone on the way back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grain {
    /// The cell under the pointer, which is a plain drag.
    Cell,
    /// The word under each end, which is a double click and whatever the hand
    /// does next.
    Word,
    /// Whole lines, which is a third click.
    Line,
}

/// How much is selected, for the row to say while a hand is still drawing it
/// and for the notice that says what a release took.
///
/// Two units rather than one because a selection inside a line and a selection
/// spanning them are different questions: how much of this line, and how many
/// lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Selected {
    /// Cells of one line, which is a drag that never left it.
    Chars(u64),
    /// Lines, both ends included.
    Lines(u64),
}

impl Selected {
    /// What a piece of copied text amounts to, for the notice that says it was
    /// taken.
    pub fn of(text: &str) -> Self {
        match text.lines().count() {
            0 | 1 => Self::Chars(text.chars().count() as u64),
            lines => Self::Lines(lines as u64),
        }
    }
}

impl std::fmt::Display for Selected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (count, thing) = match self {
            Self::Chars(chars) => (*chars, "char"),
            Self::Lines(lines) => (*lines, "line"),
        };
        write!(f, "{count} {thing}")?;
        if count != 1 {
            write!(f, "s")?;
        }
        Ok(())
    }
}

/// What became of a copy that had to wait for the lines to make it out of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Owed {
    /// None was owed: either nothing was waiting, or the release could be
    /// answered on the spot.
    Nothing,
    /// The lines arrived, and this is what was on them.
    Copied(String),
    /// They did not, and nothing later will. The block that came back is the
    /// answer to the request the press made, so a selection it does not cover
    /// is one no block after it covers either. Worth saying out loud: the
    /// gesture is over, the clipboard still holds whatever it held, and the
    /// highlight on the screen says otherwise.
    Lost,
    /// The lines arrived and there was nothing on them: a double click on a
    /// blank, which could not be told from one on a word until now, or a drag
    /// over the empty end of a line. The gesture is over and took nothing,
    /// which is what a click does, so there is nothing to say about it either.
    Empty,
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

/// What a tick of a drag held against an edge did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chased {
    /// The view moved a line, and the selection grew by one.
    Moved,
    /// There is nothing further that way, so the chase is over until the hand
    /// moves again.
    Stopped,
    /// The selection is as long as one copy can carry ([`COPY_LINES`]), so it
    /// stopped growing. Worth saying out loud: a view that stopped moving
    /// under a hand still asking it to reads as one that stopped working.
    Full,
}

/// Chase moves between one rung of the ramp and the next, and the rung it
/// stops climbing at. With [`terminal::CHASE_EVERY`] behind them these are the
/// speed: a line a move to start with, a dozen of them from two seconds in.
///
/// The rungs are what get taller rather than the clock getting faster, since a
/// move costs a repaint of the window whatever it covers: at the top of the
/// ramp that is a screenful every two frames instead of forty frames a second
/// to say the same thing.
///
/// [`terminal::CHASE_EVERY`]: crate::client::attach::terminal
const FASTER_EVERY: u64 = 4;
const FASTEST: u64 = 12;

/// Lines one move of a chase covers, by how many it has already made.
///
/// A hand held at the edge is asking for two different things at two different
/// moments: the line just above the window, which wants one line at a time and
/// the chance to let go on it, and everything back to where a build started,
/// which at one line a move is a wait with nothing to do in it. Everything else
/// with an edge to drag past reads the two apart from how far past the edge the
/// pointer went, which is the one thing a terminal will not say: it clamps what
/// it reports to the window it has. So the ramp is on how long the hand has
/// been holding, which is the other thing it is telling us.
fn step(chased: u64) -> u64 {
    (1 + chased / FASTER_EVERY).min(FASTEST)
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
    ///
    /// The size is the surface the view is drawn on, which is not the
    /// terminal's: the desktop keeps a row of it for the mark and passes what
    /// is left, and a phone drawing the view in a widget of its own passes all
    /// of it. Worked out here, as it once was, this module would be carrying a
    /// fact about one client's layout that the other one has to undo.
    pub fn new(size: Size) -> Self {
        Self {
            offset: 0,
            total: 0,
            answered: false,
            block: None,
            asked: None,
            rows: size.rows,
            cols: size.cols,
            search: None,
            selection: None,
            chasing: None,
            chased: 0,
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
        self.rows = size.rows;
        self.cols = size.cols;
        // Every row is somewhere else now, so what was painted where says
        // nothing about what is there.
        self.painted.clear();
    }

    /// How far back the view is, for the row at the bottom of the screen.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Lines the host has, screen included, as it last said. Zero until it has
    /// said, which is what [`Self::offset`] is worth reading against: the two
    /// together are how far through the history the view is sitting, and a
    /// client with somewhere to draw that wants both.
    pub fn total(&self) -> u64 {
        self.total
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
            // Saturating, because `top` parks the view at the far end before
            // there is a far end to park it at: `g` and then anything that
            // moves back, inside the round trip to the node, is an add to
            // `u64::MAX`. Which is the panic in a debug build and, worse, the
            // wrap in a release one, where the oldest line there is becomes two
            // lines above the live screen.
            self.offset = self.offset.saturating_add(lines);
            return;
        }
        let furthest = self.total.saturating_sub(self.page());
        self.offset = self.offset.saturating_add(lines).min(furthest);
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
        let mut from = self.offset.saturating_sub(below);
        // Saturating because the view can be as far back as it likes before
        // the first answer says how far back there is (see `up`).
        let mut top = from.saturating_add(span);
        // A selection can be longer than the window, since a drag held at an
        // edge scrolls the view under itself. Its text is built out of one
        // block (`holds`), so the block is stretched over it rather than the
        // copy being answered out of lines that are not here. Only when the
        // window has left the block anyway: the ends of a selection cannot
        // leave a block the window is still inside, since the end that moves
        // is the one under the pointer.
        if let Some(selection) = self.selection {
            let (first, last) = selection.ends();
            from = from.min(last.line);
            top = top.max(first.line.saturating_add(1));
        }
        let request = ViewRequest {
            from,
            lines: u32::try_from((top - from).min(COPY_LINES + span)).unwrap_or(u32::MAX),
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
    /// Saturating, like everything else that reads the offset: `top` parks it
    /// at `u64::MAX` until the host says where the far end is, so `g` and then
    /// a click, inside one round trip, is an add to that. What the click lands
    /// on is nothing anybody pointed at either way, since the screen in that
    /// window is still the one painted before the jump; a line no block will
    /// ever hold is the honest answer, and `holds` turns it into a copy of
    /// nothing rather than a copy of the wrong thing.
    fn cell(&self, spot: Spot) -> Cell {
        let rows = self.page();
        let row = u64::from(spot.row).clamp(1, rows);
        Cell {
            line: self.offset.saturating_add(rows - row),
            col: spot.col.saturating_sub(1).min(self.cols.saturating_sub(1)),
        }
    }

    /// Start a selection where the button went down, dropping whatever was
    /// selected before.
    pub fn select_from(&mut self, spot: Spot) {
        let at = self.cell(spot);
        self.stop_chasing();
        self.forget_the_copy();
        self.selection = Some(Selection {
            anchor: at,
            head: at,
            grain: Grain::Cell,
        });
    }

    /// Drag it to here. Nothing at all if no button ever went down, which is a
    /// report arriving out of order rather than a gesture.
    pub fn select_to(&mut self, spot: Spot) {
        let at = self.cell(spot);
        if self.at_an_edge(spot) {
            self.chasing = Some(spot);
        } else {
            self.stop_chasing();
        }
        if let Some(selection) = &mut self.selection {
            selection.head = at;
        }
    }

    /// Whether a drag has reached the top or bottom of the window, which is a
    /// hand asking for the lines past it.
    ///
    /// The rows either end rather than only what is beyond them: a terminal
    /// clamps the coordinates it reports to the window it has, so a pointer
    /// dragged off the bottom of the screen reports the last row it can and
    /// then says nothing more at all. An edge that only counted what is past
    /// it would be an edge nothing ever reached.
    fn at_an_edge(&self, spot: Spot) -> bool {
        spot.row <= 1 || u64::from(spot.row) >= self.page()
    }

    /// Whether a drag is being held against an edge, and so whether the view
    /// owes it a move.
    pub fn chasing(&self) -> bool {
        self.chasing.is_some()
    }

    /// Let go of the button. What is selected is untouched; what ends is the
    /// hand's claim on the view.
    pub fn let_go(&mut self) {
        self.stop_chasing();
    }

    /// The hand is no longer asking for the view, whichever way it stopped.
    /// The count goes with it, so the next chase starts slow again.
    fn stop_chasing(&mut self) {
        self.chasing = None;
        self.chased = 0;
    }

    /// Move the view the way a drag held at an edge is pointing, and take the
    /// lines that uncovers into the selection.
    ///
    /// On a clock rather than on the reports, because a hand holding still at
    /// the edge of the window is a hand the terminal has nothing to say about:
    /// the drag would stop at the last row somebody could see, which is the one
    /// row they can already reach.
    pub fn chase(&mut self) -> Chased {
        let Some(spot) = self.chasing else {
            return Chased::Stopped;
        };
        self.chased += 1;
        let step = step(self.chased);
        let was = self.offset;
        if spot.row <= 1 {
            self.up(step);
        } else {
            self.down(step);
        }
        if self.offset == was {
            // The oldest line there is, or the live screen: nothing that way.
            self.stop_chasing();
            return Chased::Stopped;
        }
        self.select_to(spot);
        let over = self.spans().saturating_sub(COPY_LINES);
        if over > 0 {
            // Give back the lines that would have made it too long to copy. A
            // selection that grew past what one block holds is one that copies
            // nothing, and stopping where it can still be taken is the only
            // answer that keeps the gesture worth making.
            if spot.row <= 1 {
                self.down(over);
            } else {
                self.up(over);
            }
            self.select_to(spot);
            self.stop_chasing();
            return Chased::Full;
        }
        Chased::Moved
    }

    /// Lines the selection covers, both ends included.
    fn spans(&self) -> u64 {
        self.selection.map_or(0, |selection| {
            let (first, last) = selection.ends();
            // Saturating for the same reason `cell` is: an end anchored while
            // the view was parked at the far end it had not yet been told
            // about is a line number nothing can be added to.
            (first.line - last.line).saturating_add(1)
        })
    }

    /// Take the word under a second click, in the sense a terminal means:
    /// letters and digits, and the punctuation that holds a path or a URL
    /// together, so that double-clicking `src/client/scroll.rs` takes all of
    /// it rather than three letters of it.
    ///
    /// The drag after it goes on taking words ([`Grain::Word`]), which is what
    /// the same gesture does everywhere else. Not where the click landed
    /// outside a word, though: there is no word under the pointer to walk by,
    /// and a grain that means nothing at the end it started from would take
    /// the blank the hand happened to stop on as the whole of a copy.
    ///
    /// Both of those are questions about the text, which on the live screen is
    /// a round trip away: the first press of the double click is what opens
    /// the view, and its block is behind the second. Asked at the press, every
    /// double click on the screen somebody is working on found no word and
    /// took one character. So nothing is asked here. The ends are left on the
    /// cell the hand pointed at and the grain is put on, and [`Self::grained`]
    /// answers both questions where it grows them, by which time the lines are
    /// here however far away the machine is.
    pub fn select_word(&mut self, spot: Spot) {
        let at = self.cell(spot);
        self.chasing = None;
        self.forget_the_copy();
        self.selection = Some(Selection {
            anchor: at,
            head: at,
            grain: Grain::Word,
        });
    }

    /// And the whole line under a third, to the end of what is on it: the
    /// blanks past that are the screen rather than the text. The drag after it
    /// takes whole lines for the same reason a double click's takes words.
    pub fn select_line(&mut self, spot: Spot) {
        let at = self.cell(spot);
        self.chasing = None;
        self.forget_the_copy();
        self.selection = Some(Selection {
            anchor: Cell {
                line: at.line,
                col: 0,
            },
            head: Cell {
                line: at.line,
                col: self.line_ends(at.line),
            },
            grain: Grain::Line,
        });
    }

    /// Drop the copy a gesture owed, its release having landed before the
    /// lines it was to be made out of.
    ///
    /// A press starts a new one, and what the last one asked for is nobody's
    /// business by then. Left standing, the block arriving mid-drag was
    /// answered out of what the *new* gesture has selected so far, which is
    /// the cell under a press: a character on the clipboard in place of
    /// whatever was about to be pasted, and the view handed back with the
    /// button still down.
    fn forget_the_copy(&mut self) {
        self.owed = false;
    }

    /// The columns of the word around a cell, both ends included, or nothing
    /// where the cell is not in a word or its line is not in the block.
    fn word_on(&self, at: Cell) -> Option<(u16, u16)> {
        let text = cells(self.line(at.line)?);
        let at = text
            .iter()
            .position(|cell| cell.col <= at.col && at.col < cell.col.saturating_add(cell.width))?;
        if !in_a_word(text[at].char) {
            return None;
        }
        let start = text[..at]
            .iter()
            .rposition(|cell| !in_a_word(cell.char))
            .map_or(0, |before| before + 1);
        let end = text[at..]
            .iter()
            .position(|cell| !in_a_word(cell.char))
            .map_or(text.len(), |after| at + after)
            - 1;
        Some((text[start].col, text[end].end()))
    }

    /// Whether the line at `offset` is here and has anything on it.
    ///
    /// [`Self::line_ends`] cannot say: it answers zero for a line with one
    /// character on it, for an empty one, and for one that has not arrived.
    fn anything_on(&self, offset: u64) -> bool {
        self.line(offset)
            .is_some_and(|line| !plain(line, 0, u16::MAX).trim_end().is_empty())
    }

    /// The last column of a line with anything on it, which is where a line
    /// selection stops: the blanks past it are the screen rather than the text.
    fn line_ends(&self, offset: u64) -> u16 {
        cells(self.line(offset).unwrap_or(""))
            .into_iter()
            .rev()
            .find(|cell| !cell.char.is_whitespace())
            .map_or(0, |cell| cell.end())
    }

    /// The two ends of a selection in reading order, each grown to the grain
    /// the gesture asked for.
    ///
    /// Grown here rather than in [`Selection`] because a word and a line are
    /// facts about the text, which lives in the block: the selection knows
    /// only where the hand went. And it widens columns and never touches line
    /// numbers, which is what lets [`Self::holds`] and [`Self::spans`] go on
    /// reading the raw ends.
    fn grained(&self, selection: &Selection) -> (Cell, Cell) {
        let (first, last) = selection.ends();
        match selection.grain {
            Grain::Cell => (first, last),
            // Outwards from each end: the one that reads first back to the
            // start of its word, the one that reads last on to the end of its
            // own. A cell in no word is left where it is, so dragging over the
            // space between two words does not swallow either of them.
            //
            // And a double click that landed on no word is not a word gesture
            // at all: there is nothing under the end it started from to walk
            // by, so it drags by the cell. Asked of the anchor here rather
            // than at the press, because at the press the lines it is asked
            // about may still be coming.
            Grain::Word if self.word_on(selection.anchor).is_some() => (
                Cell {
                    col: self.word_on(first).map_or(first.col, |(from, _)| from),
                    ..first
                },
                Cell {
                    col: self.word_on(last).map_or(last.col, |(_, to)| to),
                    ..last
                },
            ),
            Grain::Word => (first, last),
            Grain::Line => (
                Cell { col: 0, ..first },
                Cell {
                    col: self.line_ends(last.line),
                    ..last
                },
            ),
        }
    }

    /// Whether anything is selected, which is what the highlight on the screen
    /// is showing.
    pub fn selecting(&self) -> bool {
        self.selection.is_some()
    }

    /// What letting go of the button has to copy, if it has anything.
    ///
    /// A click that never became a drag selects the one cell under it, and
    /// copying a character every time somebody clicks in the window is worse
    /// than doing nothing: it throws away whatever was on the clipboard, which
    /// is often the thing they were about to paste. So a one-cell selection is
    /// read as a click and dropped, unless it is a word or a line that came out
    /// one cell wide, which is a gesture somebody made twice. Which of the two
    /// a double click was is [`Self::took`]'s to say: it needs the text, and
    /// the text is a round trip away when the click is made.
    pub fn copied(&mut self) -> Option<String> {
        let selection = self.selection?;
        if selection.grain == Grain::Cell && selection.anchor == selection.head {
            self.selection = None;
            return None;
        }
        if !self.holds(&selection) {
            self.owed = true;
            return None;
        }
        self.took()
    }

    /// The text under the highlight, and the highlight itself where there is
    /// none: a gesture that took nothing is a click, and the caller reads a
    /// selection still standing as somebody who meant to be in the view.
    ///
    /// Nothing is what a double click on a blank comes to, and on a comma, and
    /// on anything else [`in_a_word`] is not: what a hand points at in a
    /// terminal is a path or a flag, and the punctuation between two of them
    /// is a cell somebody clicked twice. It is knowable only here, the word
    /// being a fact about lines that may not have arrived when the click was
    /// made. A drag over the empty end of a line and a third click on an empty
    /// one come to the same thing by the same route.
    ///
    /// Which is why it is [`Self::selected_size`] that says whether there is
    /// anything here rather than a test of its own: that is what the mark row
    /// is showing, and a release that copies what the row says is not selected
    /// is two answers to one question. Left selected instead, all of these
    /// ended with the session paused behind a one-cell highlight and nothing
    /// to press but Esc.
    fn took(&mut self) -> Option<String> {
        let text = self.selected_size().and_then(|_| self.selected());
        if text.is_none() {
            self.selection = None;
        }
        text
    }

    /// How much is under the highlight, while anything is.
    ///
    /// What is highlighted rather than what would land on the clipboard, since
    /// this is checked against the screen: a selection dragged past the end of
    /// a short line reads here as the cells it covers, and the copy comes back
    /// shorter because the blanks past the text are trimmed off it.
    ///
    /// Nothing for the cell under a press that has not moved, which is the same
    /// answer [`Self::copied`] gives it: counting one there would put `1 char
    /// selected` on the row the instant a button went down, for a gesture that
    /// is usually a click and whose release is going to throw it away.
    pub fn selected_size(&self) -> Option<Selected> {
        let selection = self.selection?;
        // A click's word or line is the same nothing until it is found: while
        // the lines are still on their way there is nothing to measure, and on
        // a blank there is never going to be anything. Both ends sit on the
        // cell that was clicked until then, which counted as `1 char selected`
        // for a gesture that is about to say sixty.
        if selection.anchor == selection.head
            && match selection.grain {
                Grain::Cell => true,
                Grain::Word => self.word_on(selection.anchor).is_none(),
                Grain::Line => !self.anything_on(selection.anchor.line),
            }
        {
            return None;
        }
        let (first, last) = self.grained(&selection);
        Some(if first.line == last.line {
            Selected::Chars(u64::from(last.col.saturating_sub(first.col)) + 1)
        } else {
            Selected::Lines(self.spans())
        })
    }

    /// The copy that was waiting for its lines, now that a block has arrived.
    ///
    /// Answered once and then forgotten, whatever the block turned out to hold.
    /// The block on its way is the one the press asked for, so a block that
    /// does not cover the selection is a selection nothing later will cover
    /// either, and a request left standing would put a copy nobody asked for on
    /// the clipboard the next time the view moved. Which is why that failure is
    /// [`Owed::Lost`] rather than another nothing: there is a release behind it
    /// that took nothing and said nothing, and the highlight still on the
    /// screen claims otherwise.
    ///
    /// [`Owed::Empty`] is the other end of the same thing, and says the
    /// opposite: there was nothing to take. A gesture that never moved is a
    /// click wherever it landed, and a block that covers the lines and has
    /// nothing on them has answered the question rather than failed to. Both
    /// leave the clipboard alone; only one is worth a word on the row.
    pub fn owed_copy(&mut self) -> Owed {
        if !std::mem::take(&mut self.owed) {
            return Owed::Nothing;
        }
        let Some(selection) = self.selection else {
            return Owed::Nothing;
        };
        if !self.holds(&selection) {
            // Unless it never moved: the word a double click asked for is out
            // where no block reaches, so there was never a copy there to
            // lose. `g` and then a double click, inside one round trip.
            if selection.anchor == selection.head {
                self.selection = None;
                return Owed::Empty;
            }
            return Owed::Lost;
        }
        match self.took() {
            Some(text) => Owed::Copied(text),
            // The block covers the lines and there was nothing on them: the
            // word a double click asked for was a blank after all, or a drag
            // ran over the empty end of one. Nothing was lost, there being
            // nothing there to take.
            None => Owed::Empty,
        }
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
        let (first, last) = self.grained(&self.selection?);
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
    ///
    /// A line that the selection runs off the end of is selected to the edge of
    /// the screen and not one cell further: this is what is about to be drawn,
    /// and a row drawn wider than the screen wraps onto the row beneath it.
    ///
    /// The ends come in already grown to their grain rather than being worked
    /// out here, because growing them reads the text of the two lines they sit
    /// on and this is asked once per row of the window per frame: worked out
    /// here it would be two line parses a row to answer a question about the
    /// same two lines every time.
    fn selected_on(&self, ends: (Cell, Cell), offset: u64) -> Option<(u16, u16)> {
        let (first, last) = ends;
        if offset > first.line || offset < last.line {
            return None;
        }
        let from = if offset == first.line { first.col } else { 0 };
        let to = if offset == last.line {
            last.col.saturating_add(1)
        } else {
            self.cols
        };
        (from < to).then_some((from, to.min(self.cols)))
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
        // Once for the frame rather than once a row: the two ends are the same
        // two ends whichever row is being asked about.
        let ends = self.selection.map(|selection| self.grained(&selection));
        Some(
            (1..=rows)
                .map(|row| {
                    if row <= blank {
                        return String::new();
                    }
                    let line = &lines[(row - blank - 1) as usize];
                    match ends.and_then(|ends| self.selected_on(ends, self.offset + (rows - row))) {
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

/// Walk a rendered line and hand back the text of the cells from `from` up to
/// but not including `to`.
///
/// A line arrives with the pen sequences that coloured it (`node::history`
/// writes them in), so the columns a hand pointed at are not byte offsets and
/// not character offsets either. The sequences go by without counting, which is
/// the whole of the difference.
///
/// Columns are terminal cells, the same unit the node's `avt` screen uses.
/// A wide glyph occupies its two cells as one piece: a pointer on either half
/// selects the glyph rather than cutting it in two, which keeps the highlight
/// and copied text under the cursor together for CJK and emoji.
fn plain(line: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    walk(line, |piece| {
        if let Piece::Char(cell) = piece
            && cell.overlaps(from, to)
        {
            out.push(cell.char);
        }
    });
    out
}

/// The same walk, with the selected cells sat on the band ([`pen::selected`])
/// and the line's own pen put back after them.
///
/// The pen is *accumulated* rather than reissued as it arrives: what a cell
/// looks like is every SGR sequence before it on the line, so the way back to
/// the line's own colours after a band is a reset and then all of them again.
/// Inside the band they are held rather than written, since the band is worked
/// out from them and carries them itself.
///
/// Which is why the band is worked out once per pen rather than once per cell,
/// and written again whenever either of the two things it depends on moves: the
/// pen under it, and the run of selected cells starting over.
///
/// `to` is a column of the screen and never a stand-in for the end of the line:
/// what this writes goes onto a row that is only so wide, and cells written
/// past that wrap onto the row below. [`Scrollback::selected_on`] is where that
/// is made true.
fn highlighted(line: &str, from: u16, to: u16) -> String {
    let mut out = String::new();
    // The line's own pen as it stands at this point in the walk.
    let mut pen = String::new();
    // The band for that pen, while it is still the pen in hand.
    let mut band: Option<String> = None;
    let mut inside = false;
    let end = walk(line, |piece| match piece {
        Piece::Pen(seq) => {
            pen.push_str(seq);
            band = None;
            if !inside {
                out.push_str(seq);
            }
        }
        Piece::Char(cell) => {
            let wanted = cell.overlaps(from, to);
            if wanted {
                let known = band.is_some();
                let band = band.get_or_insert_with(|| pen::selected(&pen));
                if !known || !inside {
                    out.push_str(band);
                }
            } else if inside {
                out.push_str("\x1b[0m");
                out.push_str(&pen);
            }
            inside = wanted;
            out.push(cell.char);
        }
    });
    // A selection that runs past the end of the text takes the blanks with it,
    // or a dragged-over empty line would show nothing at all and a multi-line
    // selection would come apart at every short line in it.
    if to > end {
        // The cells between the end of the text and where the selection starts
        // are neither text nor selected, and something has to be written over
        // them or the band begins at the wrong column: on a line shorter than
        // `from`, and on an empty one that is column zero.
        let gap = from.saturating_sub(end);
        out.push_str(&" ".repeat(usize::from(gap)));
        // The bare band rather than the one the text was wearing: past the end
        // of the line there is no text, and an underline carried out into the
        // blanks would draw a rule across the empty half of the row.
        out.push_str(&pen::selected(""));
        out.push_str(&" ".repeat(usize::from(to.saturating_sub(end.max(from)))));
        inside = true;
    }
    if inside {
        out.push_str("\x1b[0m");
        out.push_str(&pen);
    }
    out
}

/// A rendered line comes in two kinds of piece.
enum Piece<'a> {
    /// A character, and the cells it occupies.
    Char(RenderedCell),
    /// A sequence that takes none: the pen the line was written with.
    Pen(&'a str),
}

/// One rendered character and the terminal cells it occupies.
///
/// `avt` treats every character except a Unicode-width-two one as a single
/// cell. Mirroring that rule exactly matters more than a broader notion of
/// display width: these coordinates came from the same `avt` screen.
#[derive(Debug, Clone, Copy)]
struct RenderedCell {
    char: char,
    col: u16,
    width: u16,
}

impl RenderedCell {
    fn end(self) -> u16 {
        self.col.saturating_add(self.width).saturating_sub(1)
    }

    fn overlaps(self, from: u16, to: u16) -> bool {
        self.col < to && self.col.saturating_add(self.width) > from
    }
}

/// `avt`'s cell width for a character. Keep the renderer and the source screen
/// in lockstep, including its deliberate one-cell treatment of combining and
/// other non-wide Unicode characters.
fn width(c: char) -> u16 {
    if c > '\u{7e}' && c.width().unwrap_or(1) == 2 {
        2
    } else {
        1
    }
}

/// The rendered characters in a line, without its pen sequences.
fn cells(line: &str) -> Vec<RenderedCell> {
    let mut cells: Vec<RenderedCell> = Vec::new();
    walk(line, |piece| {
        if let Piece::Char(cell) = piece {
            cells.push(cell);
        }
    });
    cells
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
            let width = width(c);
            each(Piece::Char(RenderedCell {
                char: c,
                col,
                width,
            }));
            col = col.saturating_add(width);
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

    /// The surface the view is drawn on, which on a 25 row terminal is the 24
    /// left once the mark row has its own.
    const SIZE: Size = Size { cols: 80, rows: 24 };

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

    /// `g` before the first answer parks the view at a far end nobody has
    /// named yet, and anything that moves back from there is an add to
    /// `u64::MAX`: a panic in a debug build, and in a release one the oldest
    /// line there is turning into two lines above the live screen. Both ends of
    /// the gesture happen inside one round trip to the node, so it is a key
    /// sequence rather than a race.
    #[test]
    fn moving_back_from_the_far_end_before_it_is_known_stays_at_the_end() {
        let mut view = Scrollback::new(SIZE);
        view.top();
        view.up(WHEEL);
        view.page_up();
        answer(&mut view, 100);
        assert_eq!(view.offset(), 100 - 24, "still the oldest line there is");
    }

    /// And the same parked offset reaches every other piece of arithmetic in
    /// here through `cell`, which is where the mouse comes in: `g` and then a
    /// click, or a drag, or a double click, inside that same round trip.
    /// Saturating leaves a selection on lines no block will ever hold, which
    /// `holds` reads as a copy of nothing rather than a copy of the wrong
    /// place.
    #[test]
    fn pointing_at_the_screen_before_the_far_end_is_known_copies_nothing() {
        let mut view = Scrollback::new(SIZE);
        view.top();
        view.select_from(at(24, 1));
        view.select_to(at(1, 4));
        view.select_word(at(12, 2));
        view.select_line(at(12, 2));
        // The request built from it is the other half: a selection anchored out
        // there must not overflow the block arithmetic either.
        let _ = view.wanted();
        assert_eq!(view.copied(), None, "nothing here to copy");
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

    /// A word is what a hand in a terminal points at, so the punctuation
    /// between two of them is in none: a double click on a comma is a click,
    /// and a click takes nothing. Taken, it is one character on the clipboard
    /// in place of whatever was about to be pasted, which is the whole of what
    /// [`Scrollback::copied`] is written to avoid.
    #[test]
    fn a_double_click_on_punctuation_takes_nothing() {
        let text = "a, b".to_string();
        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec![text.clone()],
        });
        view.select_word(at(24, 2));
        assert_eq!(view.copied(), None);
        assert!(!view.selecting(), "and the highlight goes with it");

        // And the same on the live screen, where the lines come after the
        // release and the answer is owed.
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 2));
        assert_eq!(view.copied(), None);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec![text],
        });
        assert_eq!(view.owed_copy(), Owed::Empty);
        assert!(!view.selecting());
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

    /// A drag held at the top row reaches the lines above it: the view moves
    /// back a line and the line that uncovers joins the selection. Without it
    /// a selection could never be longer than what was already on the screen,
    /// which is the one thing the history is there for.
    #[test]
    fn a_drag_held_at_the_top_of_the_window_reaches_the_lines_above_it() {
        let mut view = view(100);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        assert!(view.chasing(), "a drag at the top row asks for the view");
        assert_eq!(view.chase(), Chased::Moved);
        assert_eq!(view.offset(), 1);
        answer(&mut view, 100);
        let copied = view.copied().unwrap();
        assert_eq!(copied.lines().next(), Some("line 75"));
        assert_eq!(copied.lines().count(), 25, "a screenful and the one chased");
    }

    /// And at the bottom row it goes the other way, which is only somewhere to
    /// go while the view is scrolled back.
    #[test]
    fn a_drag_held_at_the_bottom_of_the_window_goes_the_other_way() {
        let mut view = view(100);
        view.up(10);
        answer(&mut view, 100);
        view.select_from(at(1, 1));
        view.select_to(at(24, 1));
        assert_eq!(view.chase(), Chased::Moved);
        assert_eq!(view.offset(), 9);
    }

    /// The live screen is as far forward as there is, and the oldest line as
    /// far back. A chase with nowhere to go stops asking rather than ticking
    /// against the end of the buffer for as long as the button is held.
    #[test]
    fn a_chase_with_nowhere_to_go_stops() {
        let mut live = view(100);
        live.select_from(at(1, 1));
        live.select_to(at(24, 1));
        assert_eq!(live.chase(), Chased::Stopped);
        assert_eq!(live.offset(), 0);
        assert!(!live.chasing());

        let mut oldest = view(30);
        oldest.top();
        oldest.select_from(at(24, 1));
        oldest.select_to(at(1, 1));
        assert_eq!(oldest.chase(), Chased::Stopped);
        assert_eq!(oldest.offset(), 30 - 24);
    }

    /// A hand that moved back off the edge is done asking, and so is one that
    /// let go. The second is the one that matters: a chase that outlived the
    /// button would go on scrolling the view under a selection nobody is
    /// making any more.
    #[test]
    fn a_chase_ends_with_the_gesture_that_started_it() {
        let mut view = view(100);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        assert!(view.chasing());
        view.select_to(at(12, 1));
        assert!(!view.chasing(), "back inside the window");

        view.select_to(at(1, 1));
        assert!(view.chasing());
        view.let_go();
        assert!(!view.chasing(), "the button came up");
        assert_eq!(view.chase(), Chased::Stopped);
    }

    /// A chase starts at a line a move and speeds up while it is held. The
    /// first move is the one that has to be small: reaching for the line just
    /// above the window is the common gesture, and a chase that opened at full
    /// speed would take a screenful before the hand could let go of it.
    #[test]
    fn a_chase_starts_slow_and_speeds_up_while_it_is_held() {
        assert_eq!(step(1), 1);
        assert_eq!(step(FASTER_EVERY), 2);
        assert_eq!(step(FASTER_EVERY * FASTEST), FASTEST, "and stops there");
        assert_eq!(step(10_000), FASTEST);

        let mut view = view(1000);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        for _ in 0..FASTER_EVERY {
            assert_eq!(view.chase(), Chased::Moved);
            answer(&mut view, 1000);
        }
        assert_eq!(
            view.offset(),
            FASTER_EVERY - 1 + 2,
            "a line each until the rung, then two"
        );
    }

    /// Letting go and starting again starts slow again, or a second drag
    /// somewhere else would open at whatever speed the first one worked up to.
    #[test]
    fn a_chase_that_ended_starts_over() {
        let mut view = view(1000);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        for _ in 0..FASTER_EVERY * 4 {
            view.chase();
            answer(&mut view, 1000);
        }
        assert!(
            view.chase() == Chased::Moved && view.offset() > 24,
            "well into it"
        );

        view.let_go();
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        let was = view.offset();
        assert_eq!(view.chase(), Chased::Moved);
        assert_eq!(view.offset() - was, 1);
    }

    /// The block stretches over a selection longer than the window, or the
    /// copy would be made out of lines that are not here and come back short.
    #[test]
    fn a_selection_longer_than_the_window_is_still_all_in_one_block() {
        let mut view = view(1000);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        while view.offset() < 100 {
            assert_eq!(view.chase(), Chased::Moved);
            answer(&mut view, 1000);
        }
        let reached = view.offset();
        let copied = view.copied().unwrap();
        assert_eq!(
            copied.lines().count(),
            reached as usize + 24,
            "the screenful and everything chased past it"
        );
        assert_eq!(
            copied.lines().next(),
            Some(&*format!("line {}", 976 - reached))
        );
    }

    /// And it stops growing where one block stops being able to hold it. A
    /// selection that outgrew what a copy can carry is one that copies
    /// nothing, so the chase ends there and says so.
    #[test]
    fn a_selection_stops_growing_at_what_a_copy_can_carry() {
        let mut view = view(5000);
        view.select_from(at(24, 1));
        view.select_to(at(1, 1));
        let mut moved = 0;
        let ended = loop {
            match view.chase() {
                Chased::Moved => {
                    moved += 1;
                    answer(&mut view, 5000);
                }
                ended => break ended,
            }
        };
        assert_eq!(ended, Chased::Full);
        assert!(
            moved < COPY_LINES - 24,
            "and it got there in fewer moves than lines, having sped up: {moved}"
        );
        assert!(!view.chasing());
        // The block the last move asked for stops where the selection did, so
        // the copy is one line past it: the release asks again, which is the
        // same round trip a copy made before its lines arrived waits out.
        answer(&mut view, 5000);
        assert_eq!(
            view.copied().unwrap().lines().count(),
            COPY_LINES as usize,
            "and what it stopped at is still copyable"
        );
    }

    /// The band goes under the line's own pen, and that pen is put back after
    /// it: what comes back off the wire is coloured text, and a copy mode that
    /// dropped the colours would repaint the screen on the way past.
    #[test]
    fn the_band_goes_under_the_colours_and_puts_them_back() {
        let coloured = "\x1b[31mred\x1b[0m plain";
        let painted = highlighted(coloured, 0, 3);
        assert!(painted.starts_with("\x1b[31m"), "{painted:?}");
        assert_eq!(plain(&painted, 0, u16::MAX), "red plain");
        // The selected cells sit on the band, and what follows is the pen the
        // line had by then, reissued from a clean slate.
        assert!(
            painted.contains(&format!("{}red\x1b[0m", pen::selected("\x1b[31m"))),
            "{painted:?}"
        );
        assert!(painted.ends_with("\x1b[31m\x1b[0m plain"), "{painted:?}");
    }

    /// The band is a background and nothing else: the line goes on saying what
    /// it said, in the colour it said it in. A selection that repainted the
    /// text would destroy the thing it was pointing at.
    #[test]
    fn the_band_keeps_the_colour_the_line_was_written_in() {
        let painted = highlighted("\x1b[32mgreen\x1b[0m out", 0, 5);
        assert!(
            painted.contains(&format!("{}green", pen::selected("\x1b[32m"))),
            "{painted:?}"
        );
    }

    /// And it follows the pen under it. A run of selected cells is not one
    /// colour: a line that changes colour halfway through changes colour
    /// halfway through the selection too.
    #[test]
    fn a_colour_that_changes_under_the_band_is_followed() {
        let painted = highlighted("\x1b[32mgr\x1b[33meen", 0, 4);
        assert!(
            painted.contains(&format!("{}gr", pen::selected("\x1b[32m"))),
            "{painted:?}"
        );
        assert!(
            painted.contains(&format!("{}ee", pen::selected("\x1b[33m"))),
            "{painted:?}"
        );
    }

    /// Reverse video is the one thing the band cannot be laid over: it swaps
    /// the two colours a cell already has, so over a run the program drew
    /// reversed itself, which is how `pi` draws the message you typed, the band
    /// would be swapped away and the selection would be invisible at exactly
    /// the place somebody was looking. So the swap is undone under it.
    #[test]
    fn a_band_over_reversed_text_is_still_visible() {
        let painted = highlighted("\x1b[7mtyped\x1b[27m out", 0, 5);
        // The band is the last thing written before the cells, and it opens
        // with a reset, so the program's reverse is off under it however it got
        // there.
        assert!(
            painted.contains(&format!("{}typed", pen::selected("\x1b[7m"))),
            "{painted:?}"
        );
        assert!(!pen::selected("\x1b[7m").contains(";7;"), "still reversed");
        // And it comes back for what was not selected.
        assert!(
            painted.ends_with("\x1b[0m\x1b[7m\x1b[27m out"),
            "{painted:?}"
        );
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

    /// A mouse report is in terminal cells, not Unicode scalar values. `avt`
    /// gives this glyph two cells, so either half has to mean the same glyph:
    /// starting on its tail used to select the following character instead.
    #[test]
    fn wide_glyphs_stay_under_either_half_of_the_cursor() {
        let line = "a界b";
        assert_eq!(plain(line, 1, 2), "界", "its first cell");
        assert_eq!(plain(line, 2, 3), "界", "and its second");
        assert_eq!(plain(line, 3, 4), "b", "the cell after it");

        let painted = highlighted(line, 2, 3);
        assert!(
            painted.contains(&format!("{}界", pen::selected(""))),
            "the whole glyph wears the band: {painted:?}"
        );
    }

    /// Word selection reads the same cell coordinates as ordinary selection:
    /// a double click on the tail of a wide character still names its word.
    #[test]
    fn a_word_under_a_wide_cursor_cell_is_not_shifted() {
        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec!["a界b".to_string()],
        });
        view.select_word(at(24, 3));
        assert_eq!(view.copied().as_deref(), Some("a界b"));
    }

    /// A selection that runs past the end of a short line takes the blanks with
    /// it, or a block dragged down the screen would come apart at every line
    /// shorter than the one above it.
    #[test]
    fn a_highlight_past_the_end_of_a_line_covers_the_rest_of_the_row() {
        let painted = highlighted("ab", 0, 6);
        assert!(
            painted.starts_with(&format!("{}ab", pen::selected(""))),
            "{painted:?}"
        );
        assert!(painted.ends_with("    \x1b[0m"), "{painted:?}");
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
        assert_eq!(view.owed_copy(), Owed::Copied("line".into()));
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
        assert!(matches!(view.owed_copy(), Owed::Copied(_)));
        assert_eq!(view.owed_copy(), Owed::Nothing);
    }

    /// Nothing is owed by a copy that could be made on the spot, or letting go
    /// twice would copy twice.
    #[test]
    fn a_copy_that_could_be_made_owes_nothing() {
        let mut view = view(100);
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        assert_eq!(view.copied().as_deref(), Some("line"));
        assert_eq!(view.owed_copy(), Owed::Nothing);
    }

    /// The block that comes back is the answer to the request the press made,
    /// so a selection it does not cover is one nothing later covers either.
    /// Told apart from nothing being owed because there is a release behind it
    /// that took nothing: unanswered, the clipboard keeps what it had and the
    /// highlight still on the screen says otherwise.
    #[test]
    fn a_copy_whose_lines_never_arrive_is_lost_rather_than_forgotten() {
        let mut view = Scrollback::new(SIZE);
        // `g` before the host has said where the far end is parks the view at
        // a line no block will ever hold.
        view.top();
        view.select_from(at(24, 1));
        view.select_to(at(24, 4));
        assert_eq!(view.copied(), None, "nothing to copy out of yet");

        answer(&mut view, 100);
        assert_eq!(view.owed_copy(), Owed::Lost);
        assert_eq!(view.owed_copy(), Owed::Nothing, "and asked once");
    }

    /// A double click takes a word, and the drag after it goes on taking them.
    /// Dragged a cell at a time instead, the gesture loses the granularity it
    /// was started with the moment the hand moves, which is the thing that
    /// makes a selection feel like it is holding on to something.
    #[test]
    fn a_drag_that_started_on_a_word_goes_on_taking_words() {
        let mut view = view(100);
        // `line 99` on the bottom row: the word under the pointer is `line`.
        view.select_word(at(24, 2));
        assert_eq!(view.copied().as_deref(), Some("line"));

        view.select_word(at(24, 2));
        // Into the first digit of `99`, which a cell-wise drag would stop at.
        view.select_to(at(24, 6));
        assert_eq!(view.copied().as_deref(), Some("line 99"));
    }

    /// And the end it started from stays whole when the hand goes the other
    /// way, which is the half a grain written into the ends as they moved
    /// could not do.
    #[test]
    fn a_word_drag_backwards_keeps_the_word_it_started_on() {
        let mut view = view(100);
        view.select_word(at(24, 7));
        assert_eq!(view.copied().as_deref(), Some("99"));

        view.select_word(at(24, 7));
        view.select_to(at(24, 3));
        assert_eq!(view.copied().as_deref(), Some("line 99"));
    }

    /// A third click takes a line, and the drag after it takes whole ones.
    #[test]
    fn a_drag_that_started_on_a_line_goes_on_taking_lines() {
        let mut view = view(100);
        view.select_line(at(24, 3));
        assert_eq!(view.copied().as_deref(), Some("line 99"));

        view.select_line(at(24, 3));
        // A row up and a few columns in, which cell-wise would cut both ends.
        view.select_to(at(23, 4));
        assert_eq!(view.copied().as_deref(), Some("line 98\nline 99"));
    }

    /// A double click that landed on no word has no word to walk by, so the
    /// drag after it is an ordinary one. Kept at the grain instead, the blank
    /// the hand happened to stop on would be the whole of the copy.
    #[test]
    fn a_double_click_off_a_word_drags_by_the_cell() {
        let mut view = view(100);
        // Column five of `line 99` is the space between the two words.
        view.select_word(at(24, 5));
        assert_eq!(view.copied(), None, "a cell nobody dragged out of");

        view.select_word(at(24, 5));
        view.select_to(at(24, 6));
        assert_eq!(view.copied().as_deref(), Some(" 9"));
    }

    /// Whether there was a word under it is asked of the text, so it cannot be
    /// asked while the lines are still coming: the same gesture on the live
    /// screen has to answer the same way, or a double click drags by the word
    /// or by the cell depending on how far away the machine is.
    #[test]
    fn a_double_click_off_a_word_drags_by_the_cell_whenever_the_lines_arrive() {
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 5));
        view.select_to(at(24, 6));
        answer(&mut view, 100);
        assert_eq!(view.copied().as_deref(), Some(" 9"));
    }

    /// The double click nobody could make: on the live screen the press that
    /// opens the view is a round trip ahead of the lines it opens on, so the
    /// second one lands with no block to look for a word in. Read as no word
    /// at all it took the cell under the pointer, which is a double click that
    /// selects one character.
    #[test]
    fn a_double_click_takes_its_word_out_of_a_block_that_had_not_arrived() {
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 2));
        assert_eq!(view.copied(), None, "nothing to take it out of yet");

        answer(&mut view, 100);
        assert_eq!(view.owed_copy(), Owed::Copied("line".to_string()));
    }

    /// And it may turn out to have landed on nothing, which is only knowable
    /// once the lines are here. A click takes nothing; so does this, rather
    /// than the blank it landed on or a notice saying a copy was lost.
    #[test]
    fn a_double_click_on_a_blank_takes_nothing_once_the_lines_arrive() {
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 5));
        assert_eq!(view.copied(), None);

        answer(&mut view, 100);
        assert_eq!(view.owed_copy(), Owed::Empty);
        assert!(!view.selecting(), "and the highlight goes with it");
    }

    /// And the lines usually beat the button up: a release is a hand's tens of
    /// milliseconds away and a block is a round trip, which on a local node is
    /// nothing at all. So the same double click on a blank has to be read as a
    /// click on the way out of `copied` as well, or the release takes nothing,
    /// says nothing, and leaves the view up over a session that has stopped
    /// being painted with a one-cell highlight on it.
    #[test]
    fn a_double_click_on_a_blank_lets_go_when_the_lines_beat_the_button_up() {
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 5));
        answer(&mut view, 100);
        assert_eq!(view.copied(), None);
        assert!(!view.selecting(), "and the view is handed back");
    }

    /// Which is the general shape of it: a selection whose cells are blank
    /// takes nothing whatever gesture made it, and a highlight over nothing is
    /// not a reason to stay in the view.
    #[test]
    fn a_selection_with_nothing_on_it_lets_go_of_the_view() {
        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 2,
            lines: vec![String::new(), "        ".to_string()],
        });
        view.select_line(at(24, 3));
        assert_eq!(view.copied(), None, "an empty line has nothing on it");
        assert!(!view.selecting());

        view.select_from(at(24, 2));
        view.select_to(at(24, 6));
        assert_eq!(view.copied(), None, "and neither have blanks dragged over");
        assert!(!view.selecting());
    }

    /// The same answer when the lines arrive late. A block that covers the
    /// selection and has nothing on it is not a copy that was lost: nothing
    /// was there to take, which is what a click does and what a click says.
    #[test]
    fn an_owed_copy_of_blanks_is_nothing_rather_than_a_copy_lost() {
        let mut view = Scrollback::new(SIZE);
        view.select_from(at(24, 3));
        view.select_to(at(24, 9));
        assert_eq!(view.copied(), None, "nothing to take it out of yet");

        view.take(Window {
            from: 0,
            total: 3,
            lines: vec![String::new(); 3],
        });
        assert_eq!(view.owed_copy(), Owed::Empty);
        assert!(!view.selecting());
    }

    /// A press starts a gesture, and the one before it is over: a copy its
    /// release could not be answered out of an empty block belongs to that
    /// gesture and to nothing after it. Left standing, the block arriving
    /// mid-drag answered it out of whatever is selected *now*, which is a
    /// character on the clipboard and the view handed back with the button
    /// still down.
    #[test]
    fn a_new_press_drops_the_copy_the_last_one_never_finished() {
        let mut view = Scrollback::new(SIZE);
        view.select_word(at(24, 2));
        assert_eq!(view.copied(), None, "owed until the lines arrive");

        view.select_line(at(24, 2));
        answer(&mut view, 100);
        assert_eq!(view.owed_copy(), Owed::Nothing, "the double click's, gone");
        assert_eq!(
            view.copied().as_deref(),
            Some("line 99"),
            "and the release of the gesture in hand says what to take"
        );
    }

    /// And a gesture whose lines no block will ever hold has lost nothing if
    /// it never moved: `g` and a double click inside the same round trip are a
    /// click on a screen nobody has been told the size of.
    #[test]
    fn a_double_click_out_where_no_block_reaches_lost_nothing() {
        let mut view = Scrollback::new(SIZE);
        view.top();
        view.select_word(at(24, 2));
        assert_eq!(view.copied(), None);

        answer(&mut view, 100);
        assert_eq!(view.owed_copy(), Owed::Empty);
        assert!(!view.selecting(), "and the highlight goes with it");
    }

    /// What the row says while a hand is still drawing a selection. Counted in
    /// the units the selection is in: how much of this line, or how many lines.
    #[test]
    fn a_selection_is_counted_in_chars_until_it_leaves_a_line() {
        let mut view = view(100);
        assert_eq!(view.selected_size(), None, "nothing selected yet");

        view.select_from(at(24, 1));
        assert_eq!(
            view.selected_size(),
            None,
            "and a press that has not moved is a click, not a char"
        );

        view.select_to(at(24, 4));
        assert_eq!(view.selected_size(), Some(Selected::Chars(4)));

        view.select_to(at(22, 4));
        assert_eq!(view.selected_size(), Some(Selected::Lines(3)));
    }

    /// And it counts what the grain will take, not where the hand happens to
    /// be: the highlight on the screen is already showing the whole word.
    #[test]
    fn a_word_selection_is_counted_whole() {
        let mut view = view(100);
        view.select_word(at(24, 2));
        assert_eq!(view.selected_size(), Some(Selected::Chars(4)));

        // And says nothing about a word that turned out not to be there, or
        // about one nobody can see yet: `1 char selected` is what a click
        // would say, and a click says nothing.
        view.select_word(at(24, 5));
        assert_eq!(view.selected_size(), None, "a blank is not a word");

        let mut coming = Scrollback::new(SIZE);
        coming.select_word(at(24, 2));
        assert_eq!(coming.selected_size(), None, "the lines are still coming");
        answer(&mut coming, 100);
        assert_eq!(coming.selected_size(), Some(Selected::Chars(4)));
    }

    /// And a line selection is the same question again: a third click says
    /// nothing about a line it cannot see, and nothing about an empty one.
    #[test]
    fn a_line_selection_is_counted_once_there_is_a_line_to_count() {
        let mut view = Scrollback::new(SIZE);
        view.select_line(at(24, 2));
        assert_eq!(view.selected_size(), None, "the lines are still coming");
        answer(&mut view, 100);
        assert_eq!(view.selected_size(), Some(Selected::Chars(7)), "line 99");

        let mut view = Scrollback::new(SIZE);
        view.take(Window {
            from: 0,
            total: 1,
            lines: vec![String::new()],
        });
        view.select_line(at(24, 2));
        assert_eq!(view.selected_size(), None, "nothing on the line");
    }

    /// One of anything is one of it. Said in the row and in the notice that
    /// answers a release, so it is worth spelling.
    #[test]
    fn an_amount_takes_the_plural_it_needs() {
        assert_eq!(Selected::Chars(1).to_string(), "1 char");
        assert_eq!(Selected::Chars(12).to_string(), "12 chars");
        assert_eq!(Selected::Lines(1).to_string(), "1 line");
        assert_eq!(Selected::Lines(9).to_string(), "9 lines");
        assert_eq!(Selected::of("one line").to_string(), "8 chars");
        assert_eq!(Selected::of("two\nlines").to_string(), "2 lines");
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
        assert!(painted.contains(&pen::selected("")), "{painted:?}");
        assert_eq!(painted.matches("\x1b[K").count(), 1, "one row: {painted:?}");
    }

    /// A row is as wide as the screen. Padded past that, the cells wrap onto
    /// the row below and the one after it: a selection over a blank line put a
    /// thousand cells of reverse video on the screen, which on a wide window is
    /// six rows of white with the text that was under them gone. Only the rows
    /// that changed are painted, so nothing comes back to tidy that up.
    #[test]
    fn a_highlight_stops_at_the_edge_of_the_screen() {
        let mut view = view(100);
        // Two rows, so the upper one is selected to the end of the line.
        view.select_from(at(23, 3));
        view.select_to(at(24, 5));
        let painted = view.paint();
        let widest = painted
            .split("\x1b[K")
            .map(|row| plain(row, 0, u16::MAX).chars().count())
            .max()
            .unwrap_or(0);
        assert!(
            widest <= usize::from(SIZE.cols),
            "{widest} cells: {painted:?}"
        );
    }

    /// And it starts at the column the hand pointed at. On a line shorter than
    /// that column, the cells in between are the ones nobody selected, so the
    /// highlight on an empty row began at column zero and ran the width of the
    /// screen.
    #[test]
    fn a_highlight_past_a_short_line_starts_where_the_selection_does() {
        let painted = highlighted("ab", 5, 9);
        assert!(
            painted.starts_with(&format!("ab   {}", pen::selected(""))),
            "{painted:?}"
        );
        assert!(painted.ends_with("    \x1b[0m"), "{painted:?}");
        assert_eq!(plain(&painted, 0, u16::MAX).chars().count(), 9);
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
