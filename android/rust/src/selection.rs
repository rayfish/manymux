//! Text from a frozen set of rendered rows, addressed in terminal cells.

use unicode_width::UnicodeWidthChar;

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
            let left = if row.at == start.row {
                usize::from(start.col)
            } else {
                0
            };
            let right = if row.at == end.row {
                usize::from(end.col) + 1
            } else {
                usize::MAX
            };
            let mut col = 0;
            let mut text = String::new();
            for ch in row
                .runs
                .into_iter()
                .flat_map(|run| run.text.chars().collect::<Vec<_>>())
            {
                let width = ch.width().unwrap_or(0);
                if col < right && col + width > left || width == 0 && !text.is_empty() {
                    text.push(ch);
                }
                col += width;
            }
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
}
