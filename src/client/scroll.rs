//! Looking back through what a session printed, on a screen the terminal keeps
//! no scrollback for.
//!
//! Only `--screen alternate` needs this. Inline the terminal has the lines in
//! its own buffer and its own wheel scrolls them, which is better than anything
//! here; this is what the other mode has instead.
//!
//! Deliberately not tmux's copy mode. There is no selection and no yank: the
//! terminal's own selection works on whatever the view is showing, which is
//! what copying a line you scrolled to actually needs. What is left is moving a
//! window over the node's ten thousand lines, which is arithmetic and string
//! building, and is all here so it can be tested without a terminal.
//!
//! The one thing that gets in the way of that selection is the wheel, since a
//! terminal reporting the mouse to us is a terminal not selecting with it. The
//! reports are worth it anyway (`attach::wheel_is_ours`): this screen is the
//! terminal's alternate one, which keeps no scrollback and has no wheel of its
//! own to fall back on, so not asking for them left the gesture reaching
//! nobody. A drag still selects under the modifier terminals keep for exactly
//! this, shift almost everywhere and option in iTerm2.
//!
//! The window moves locally inside a block that has already arrived, and a
//! block is a few screenfuls. A request per wheel notch would be a round trip
//! over ssh per wheel notch, on machines that are usually two hops away.

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

    /// The screen as it should now look: every row of the session's part of it,
    /// each clearing what was on it as it is written.
    ///
    /// Row by row rather than an erase and a repaint. The erase left the screen
    /// blank for as long as it took the lines after it to be drawn, which at
    /// one notch is nothing and at a wheel being spun is a flicker on every
    /// frame. Writing each row over the one before it never shows an empty
    /// screen, and clearing to the end of each line is what keeps a long line
    /// from showing through under a shorter one that replaced it.
    ///
    /// Nothing at all while the first block is on its way, rather than a blank
    /// screen: what is up is the session, one frame more of it is no lie, and
    /// blanking the screen to wait is the flicker again with nothing to show
    /// for it.
    pub fn paint(&self) -> String {
        let Some(block) = &self.block else {
            return String::new();
        };
        if !self.covered() {
            return String::new();
        }
        let rows = self.page();
        let lines = block.window(self.offset, rows);
        // A window at the very top of a short buffer has fewer lines than the
        // screen has rows, and they go at the bottom of it: the oldest line
        // sits where it would have been had you scrolled to it.
        let blank = rows.saturating_sub(lines.len() as u64);
        let mut out = String::from("\x1b[0m");
        for row in 1..=rows {
            // The pen goes back to default per row: the line about to be
            // written sets its own colours, and the clear that precedes it
            // would otherwise clear to whatever the line above left set.
            out.push_str(&format!("\x1b[{row};1H\x1b[0m\x1b[K"));
            if row > blank {
                out.push_str(&lines[(row - blank - 1) as usize]);
            }
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
        let view = view(3);
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
        let view = Scrollback::new(SIZE);
        assert_eq!(view.paint(), "");
    }
}
