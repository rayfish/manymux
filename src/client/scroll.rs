//! Looking back through what a session printed, on a screen the terminal keeps
//! no scrollback for.
//!
//! Only `--screen alternate` needs this. Inline the terminal has the lines in
//! its own buffer and its own wheel scrolls them, which is better than anything
//! here; this is what the other mode has instead.
//!
//! Deliberately not tmux's copy mode. There is no selection and no yank: the
//! terminal's own selection still works on whatever the view is showing, which
//! is what copying a line you scrolled to actually needs. What is left is
//! moving a window over the node's ten thousand lines, which is arithmetic and
//! string building, and is all here so it can be tested without a terminal.
//!
//! The window moves locally inside a block that has already arrived, and a
//! block is a few screenfuls. A request per wheel notch would be a round trip
//! over ssh per wheel notch, on machines that are usually two hops away.

use crate::client::status::session_size;
use crate::proto::{Size, View as Window, ViewRequest};

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
    total: u64,
    /// What we have: where its bottom sits, and its lines, oldest first.
    block: Option<Block>,
    /// The last thing asked for, so a window the host cannot fill any better
    /// is not asked for again and again. Its answer arriving does not clear
    /// this: the answer is the best there is.
    asked: Option<ViewRequest>,
    /// Rows the session's part of the screen has, which is what a page is.
    rows: u16,
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
        // newest, so the two run in opposite directions.
        let end = self.lines.len() as u64 - (offset - self.from);
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
            block: None,
            asked: None,
            rows: session_size(size).rows,
        }
    }

    pub fn resize(&mut self, size: Size) {
        self.rows = session_size(size).rows;
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
    pub fn top(&mut self) {
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
        // A buffer that trimmed under the request, or one shorter than a
        // screen: keep the window inside what exists.
        let furthest = self.total.saturating_sub(self.page());
        self.offset = self.offset.min(furthest);
        self.block = Some(Block {
            from: window.from,
            lines: window.lines,
        });
    }

    /// The screen as it should now look: every row of the session's part of it,
    /// erased first, with the cursor left where nothing will type over it.
    ///
    /// Blank while the first block is still on its way, because painting the
    /// live screen underneath a half-drawn view reads as a bug rather than a
    /// wait.
    pub fn paint(&self) -> String {
        let rows = self.page();
        let mut out = String::from("\x1b[0m\x1b[H\x1b[2J");
        if !self.covered() {
            return out;
        }
        let Some(block) = &self.block else {
            return out;
        };
        let lines = block.window(self.offset, rows);
        // A window at the very top of a short buffer has fewer lines than the
        // screen has rows, and they go at the bottom of it: the oldest line
        // sits where it would have been had you scrolled to it.
        let blank = rows.saturating_sub(lines.len() as u64);
        for (row, line) in lines.iter().enumerate() {
            let row = blank + row as u64 + 1;
            out.push_str(&format!("\x1b[{row};1H\x1b[0m{line}"));
        }
        out
    }
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
        assert!(painted.starts_with("\x1b[0m\x1b[H\x1b[2J"), "{painted:?}");
        // The window ends one line back from the newest, so its last line is
        // line 98 of 0..100, and it goes on the last row the session has.
        assert!(painted.contains("\x1b[24;1H\x1b[0mline 98"), "{painted:?}");
        assert!(painted.contains("\x1b[1;1H\x1b[0mline 75"), "{painted:?}");
        assert!(!painted.contains("line 99"), "{painted:?}");
    }

    /// At the top of a buffer shorter than the screen there is nothing to put
    /// on the first rows, and what there is belongs at the bottom.
    #[test]
    fn a_short_buffer_paints_against_the_bottom_of_the_screen() {
        let view = view(3);
        let painted = view.paint();
        assert!(painted.contains("\x1b[22;1H\x1b[0mline 0"), "{painted:?}");
        assert!(painted.contains("\x1b[24;1H\x1b[0mline 2"), "{painted:?}");
    }

    /// Until the first block lands there is nothing to draw, and drawing the
    /// live screen instead would read as the key not having worked.
    #[test]
    fn a_view_with_nothing_in_it_yet_paints_an_empty_screen() {
        let view = Scrollback::new(SIZE);
        assert_eq!(view.paint(), "\x1b[0m\x1b[H\x1b[2J");
    }
}
