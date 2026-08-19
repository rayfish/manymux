//! Scrolling back through what the session printed.
//!
//! The history lives at the node, which is the whole reason this is a request
//! and not a buffer: `screen::BEHIND` is zero, so nothing behind the visible
//! rows is kept here at all. `manymux::client::scroll::Scrollback` is the
//! arithmetic of it, shared with the desktop, and it hands back the window as
//! the escape sequences a terminal would be sent.
//!
//! Which is where this end differs, and it is the same difference the screen
//! makes: the desktop has a terminal in front of somebody to paint those into,
//! and a phone has none, so they are painted into an `avt::Vt` of the widget's
//! own size and read back out as rows of runs. Both surfaces of this client are
//! therefore rendered by the same emulator through the same `runs_of`, and a
//! colour or a wide character cannot come out one way on the screen and another
//! way two lines above it.

use std::collections::BTreeSet;

use avt::Vt;
use manymux::client::scroll::Scrollback;
use manymux::proto::{Size, View, ViewRequest};

use crate::screen::{Row, runs_of};

/// The view, as the app sees it.
#[derive(uniffi::Record)]
pub struct Window {
    /// Whether the view is up. False is the live screen.
    pub open: bool,
    /// Whether these rows are worth drawing yet.
    ///
    /// False while the first block is on its way, where what is on the screen
    /// is the session and one frame more of it is no lie. Blanking to wait
    /// would be a flicker with nothing to show for it.
    pub showing: bool,
    /// How far back from the newest line the bottom row sits.
    pub from: u64,
    /// Lines the host has, as it last said. Zero until it has said.
    pub total: u64,
    /// Only the rows that changed, the same as a [`crate::screen::Frame`].
    pub changed: Vec<Row>,
}

/// A view over the session's history, and the emulator it is painted into.
pub struct Scrolling {
    open: bool,
    back: Scrollback,
    vt: Vt,
    changed: BTreeSet<usize>,
    size: Size,
    /// Whether anything has been painted since the view opened.
    showing: bool,
}

impl Scrolling {
    pub fn at(size: Size) -> Self {
        Self {
            open: false,
            back: Scrollback::new(size),
            vt: emulator(size),
            changed: BTreeSet::new(),
            size,
            showing: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// Whether the view is back at the live screen, which is where leaving it
    /// costs nothing.
    pub fn at_bottom(&self) -> bool {
        self.back.at_bottom()
    }

    /// Move back through the history, opening the view if it was not up.
    ///
    /// Opening and moving in one gesture, because on a phone there is no key
    /// to open it with: the gesture that asks for the history is a drag, and a
    /// drag whose first inch does nothing reads as a screen that does not
    /// scroll.
    pub fn up(&mut self, lines: u64) {
        self.open = true;
        self.back.up(lines);
        self.paint();
    }

    pub fn down(&mut self, lines: u64) {
        if !self.open {
            return;
        }
        self.back.down(lines);
        self.paint();
    }

    /// Back to the live screen, and everything about the view forgotten.
    ///
    /// Forgotten rather than kept where it was: a view opened again is opened
    /// at the bottom, since what somebody is looking for after coming back to
    /// the session is not where they were the last time they went away from
    /// it. It also means the emulator and the rows the app holds start
    /// together, which is what makes an empty frame safe to skip drawing.
    pub fn close(&mut self) {
        *self = Self::at(self.size);
    }

    /// The screen is a different shape, so every row is somewhere else.
    pub fn resize(&mut self, size: Size) {
        self.size = size;
        self.back.resize(size);
        self.vt = emulator(size);
        self.changed.extend(0..size.rows as usize);
        self.paint();
    }

    /// What to ask the host for, if what is held does not cover the window.
    pub fn wanted(&mut self) -> Option<ViewRequest> {
        if !self.open {
            return None;
        }
        self.back.wanted()
    }

    /// A block the host sent.
    pub fn took(&mut self, view: View) {
        self.back.take(view);
        self.paint();
    }

    /// The view as it should now look, and only the rows that changed.
    pub fn take_window(&mut self) -> Window {
        let changed = std::mem::take(&mut self.changed)
            .into_iter()
            .filter(|at| *at < self.size.rows as usize)
            .map(|at| Row {
                at: at as u16,
                runs: runs_of(self.vt.line(at)),
            })
            .collect();
        Window {
            open: self.open,
            showing: self.showing,
            from: self.back.offset(),
            total: self.back.total(),
            changed,
        }
    }

    /// Paint whatever the view owes into the emulator standing in for a
    /// terminal.
    ///
    /// Nothing at all while there is no block to draw from, which is what
    /// `showing` says: the app is still drawing the session, and that is the
    /// right thing for it to be drawing.
    fn paint(&mut self) {
        let painted = self.back.paint();
        if painted.is_empty() {
            return;
        }
        self.showing = true;
        let changes = self.vt.feed_str(&painted);
        self.changed.extend(changes.lines.iter().copied());
    }
}

fn emulator(size: Size) -> Vt {
    Vt::builder()
        .size(size.cols as usize, size.rows as usize)
        // The view is a window over the host's history and keeps none of its
        // own, the same as the screen beside it.
        .scrollback_limit(0)
        .build()
}

#[cfg(test)]
mod tests {
    use super::Scrolling;
    use crate::screen::Colour;
    use manymux::proto::{Size, View};

    const SIZE: Size = Size { cols: 20, rows: 4 };

    fn scrolling() -> Scrolling {
        Scrolling::at(SIZE)
    }

    /// The host, standing in: answers whatever the view asks for out of a
    /// buffer of numbered lines, clamped the way `node::history::window`
    /// clamps it.
    ///
    /// Until there is nothing left to ask, which is what the app does too: a
    /// view thrown further back than the history goes is clamped when the
    /// first answer says where the end is, and the window it lands on is
    /// somewhere the block it just took may not reach.
    fn answer(view: &mut Scrolling, total: u64) {
        while let Some(request) = view.wanted() {
            let from = request.from.min(total.saturating_sub(1));
            let bottom = total - from;
            let top = bottom.saturating_sub(u64::from(request.lines));
            view.took(View {
                from,
                total,
                lines: (top..bottom).map(|i| format!("line {i}")).collect(),
            });
        }
    }

    /// What a window reads as, row by row, with the rows it did not mention
    /// left empty.
    ///
    /// Trailing blanks are trimmed: a row is as wide as the screen and the
    /// widget draws the whole of it, which says nothing about what is written
    /// on it.
    fn rows_of(view: &mut Scrolling) -> Vec<String> {
        let window = view.take_window();
        let mut rows = vec![String::new(); SIZE.rows as usize];
        for row in window.changed {
            let text: String = row.runs.iter().map(|run| run.text.as_str()).collect();
            rows[row.at as usize] = text.trim_end().to_string();
        }
        rows
    }

    #[test]
    fn a_drag_opens_the_view_and_moves_it_in_one_gesture() {
        let mut view = scrolling();
        assert!(!view.is_open());
        view.up(2);
        assert!(view.is_open());
        assert!(!view.at_bottom());
    }

    #[test]
    fn nothing_is_shown_until_the_first_block_arrives() {
        let mut view = scrolling();
        view.up(2);
        let window = view.take_window();
        assert!(window.open);
        assert!(
            !window.showing,
            "the session is still on the screen and still true"
        );
        assert!(window.changed.is_empty());
    }

    #[test]
    fn a_window_of_history_comes_back_as_rows_the_widget_can_draw() {
        let mut view = scrolling();
        view.up(2);
        answer(&mut view, 20);
        let window = view.take_window();
        assert!(window.showing);
        assert_eq!(window.from, 2);
        assert_eq!(window.total, 20);

        let mut view = scrolling();
        view.up(2);
        answer(&mut view, 20);
        // Twenty lines, the newest being `line 19`. Two back from it is the
        // bottom row, and the screen is four rows tall.
        assert_eq!(
            rows_of(&mut view),
            ["line 14", "line 15", "line 16", "line 17"]
        );
    }

    /// The whole point of asking a block at a time: moving inside what is
    /// already here costs no round trip.
    #[test]
    fn moving_inside_the_block_asks_the_host_for_nothing() {
        let mut view = scrolling();
        view.up(2);
        answer(&mut view, 200);
        view.up(1);
        assert!(
            view.wanted().is_none(),
            "a line's move is inside the block already here"
        );
        assert_eq!(
            rows_of(&mut view),
            ["line 193", "line 194", "line 195", "line 196"]
        );
    }

    #[test]
    fn only_the_rows_that_changed_come_back() {
        let mut view = scrolling();
        view.up(2);
        answer(&mut view, 200);
        assert_eq!(view.take_window().changed.len(), 4, "the first frame");
        assert!(
            view.take_window().changed.is_empty(),
            "and nothing has moved since"
        );
        view.up(1);
        assert_eq!(
            view.take_window().changed.len(),
            4,
            "every row is a different line now"
        );
    }

    /// A window at the top of a short history has fewer lines than the screen
    /// has rows, and they go at the bottom of it: the oldest line sits where
    /// it would have been had you scrolled to it.
    #[test]
    fn the_top_of_a_short_history_sits_at_the_bottom_of_the_screen() {
        let mut view = scrolling();
        view.up(50);
        answer(&mut view, 3);
        assert_eq!(rows_of(&mut view), ["", "line 0", "line 1", "line 2"]);
    }

    /// The lines arrive with the pen sequences that coloured them, which is
    /// why they are painted rather than read.
    #[test]
    fn the_colour_a_line_was_printed_in_survives_the_trip() {
        let mut view = scrolling();
        view.up(1);
        view.wanted();
        // Eight lines ending one back from the newest, so the coloured one is
        // the bottom row of the window.
        let mut lines: Vec<String> = (0..7).map(|i| format!("line {i}")).collect();
        lines.push("\x1b[0;31mred\x1b[0m plain".to_string());
        view.took(View {
            from: 1,
            total: 9,
            lines,
        });
        let window = view.take_window();
        let row = window
            .changed
            .iter()
            .find(|row| row.at == SIZE.rows - 1)
            .expect("the bottom row");
        assert_eq!(row.runs[0].text, "red");
        assert_eq!(row.runs[0].look.foreground, Colour::Indexed { index: 1 });
        // The rest of the row is the blank the screen was built from, which
        // carries the same pen and so is the same run.
        assert_eq!(row.runs[1].text.trim_end(), " plain");
        assert_eq!(row.runs[1].look.foreground, Colour::Default);
    }

    /// Leaving the view is leaving it: what it was showing is not what
    /// somebody coming back to the session an hour later is looking for.
    #[test]
    fn closing_the_view_puts_it_back_at_the_bottom() {
        let mut view = scrolling();
        view.up(30);
        answer(&mut view, 200);
        view.close();
        assert!(!view.is_open());
        assert!(view.at_bottom());
        assert!(view.wanted().is_none(), "there is nothing to draw yet");
        let window = view.take_window();
        assert!(!window.open);
        assert!(!window.showing);
    }

    /// A view still up when the keyboard opens is one every row of which has
    /// moved, so all of them are owed however few lines changed.
    #[test]
    fn a_resize_repaints_the_whole_view() {
        let mut view = scrolling();
        view.up(2);
        answer(&mut view, 200);
        let _ = view.take_window();
        view.resize(Size::new(20, 3));
        answer(&mut view, 200);
        assert_eq!(view.take_window().changed.len(), 3);
    }
}
