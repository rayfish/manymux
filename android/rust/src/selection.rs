//! Text from a frozen set of rendered rows, addressed in terminal cells.
//!
//! The cells are counted by `manymux::client::scroll::plain`, the desktop's
//! own, which counts them the way `avt` laid them out: a wide character is two
//! and everything else one, a combining mark included.

use manymux::client::scroll::plain;

use crate::mouse::At;
use crate::screen::Row;

#[uniffi::export]
pub fn selection_text(mut rows: Vec<Row>, start: At, end: At) -> String {
    let (start, end) = if (start.row, start.col) <= (end.row, end.col) {
        (start, end)
    } else {
        (end, start)
    };
    rows.sort_by_key(|row| row.at);
    rows.into_iter()
        .filter(|row| row.at >= start.row && row.at <= end.row)
        .map(|row| {
            let left = if row.at == start.row { start.col } else { 0 };
            let right = if row.at == end.row {
                end.col.saturating_add(1)
            } else {
                u16::MAX
            };
            let line: String = row.runs.into_iter().map(|run| run.text).collect();
            let text = plain(&line, left, right);
            text.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::Screen;
    use manymux::proto::Size;

    #[test]
    fn selects_by_cells_in_both_directions() {
        let mut screen = Screen::at(Size::new(12, 3));
        screen.repaint("ab界cd\r\nsecond".as_bytes());
        let rows = screen.take_frame().changed;
        assert_eq!(
            selection_text(rows, At { col: 2, row: 1 }, At { col: 3, row: 0 }),
            "界cd\nsec"
        );
    }

    /// `avt` gives a combining mark a cell of its own, so the columns the app
    /// selected by are counted the same way here or the copy lands one short.
    #[test]
    fn a_combining_mark_takes_the_cell_avt_gave_it() {
        let mut screen = Screen::at(Size::new(12, 1));
        screen.repaint("e\u{301}xyz".as_bytes());
        let rows = screen.take_frame().changed;
        assert_eq!(
            selection_text(rows, At { col: 2, row: 0 }, At { col: 3, row: 0 }),
            "xy"
        );
    }
}
