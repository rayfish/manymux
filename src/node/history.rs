//! The session's scrollback, written out the way the program printed it.
//!
//! `avt` keeps the lines that scrolled off the top of the screen, but a client
//! has no way to see them: the screen dump paints the screen and nothing else.
//! This is the rest of the model, rendered as text with the pen sequences that
//! coloured it, so an inline client can push it into its terminal's own
//! scrollback and let the terminal do the scrolling.
//!
//! Only what is behind the screen: the dump is about to paint the rest, and a
//! line painted twice is a line you scroll past twice.

use avt::{Color, Line, Pen, Vt};

use crate::proto::{Found, View, ViewRequest};

/// The last `lines` lines of history, rendered with the pen sequences that
/// coloured them, each ending in a carriage return and a newline.
///
/// The lines a wrapped line was folded into come out as separate lines, because
/// `avt` does not say which is which. That costs a long line its reflow in the
/// terminal's scrollback and nothing else.
pub fn render(vt: &Vt, lines: usize) -> String {
    let (_, rows) = vt.size();
    let all: Vec<&Line> = vt.lines().collect();
    // The view is the last `rows` of them, and the dump paints those.
    let history = all.len().saturating_sub(rows);
    let mut out = String::new();
    for line in &all[history.saturating_sub(lines)..history] {
        write_line(line, &mut out);
    }
    out
}

/// A window of the buffer for a client scrolling back through it, screen
/// included: scrolling up from the bottom has to be continuous, and the screen
/// is where the bottom is.
///
/// Both ends are clamped rather than refused. A client asking past the top of a
/// buffer that has trimmed under it gets what is there, and the `total` it
/// comes back with is how it learns where the end went.
pub fn window(vt: &Vt, request: &ViewRequest) -> View {
    let all: Vec<&Line> = vt.lines().collect();
    let total = all.len() as u64;
    let from = request.from.min(total.saturating_sub(1));
    // `from` counts back from the newest line, so the window ends here.
    let bottom = total.saturating_sub(from);
    let top = bottom.saturating_sub(u64::from(request.lines));
    let lines = all[top as usize..bottom as usize]
        .iter()
        .map(|line| {
            let mut rendered = String::new();
            write_line(line, &mut rendered);
            // The client places every line itself, so it wants the text and
            // not the newline that would move the cursor for it.
            rendered.trim_end_matches("\r\n").to_string()
        })
        .collect();
    View { from, total, lines }
}

/// Every line of the buffer holding `needle`, as offsets back from the newest
/// line, nearest the bottom first.
///
/// All of them, not a page of them: ten thousand lines is nothing to walk here,
/// and sending the lot is what lets a client step through the matches without
/// asking again. An empty needle finds nothing rather than everything, since
/// what it means is that nobody has typed anything yet.
///
/// Smartcase, the way `less` and vim do it: an all-lowercase needle ignores
/// case, and one with a capital in it means the capital.
pub fn find(vt: &Vt, needle: &str) -> Found {
    let mut lines = Vec::new();
    if needle.is_empty() {
        return Found {
            needle: needle.to_string(),
            lines,
        };
    }
    let folded = needle.to_lowercase();
    let cased = needle != folded;
    let all: Vec<&Line> = vt.lines().collect();
    let total = all.len() as u64;
    for (i, line) in all.iter().enumerate() {
        let text = line.text();
        let found = if cased {
            text.contains(needle)
        } else {
            text.to_lowercase().contains(&folded)
        };
        if found {
            // The offset that puts this line at the bottom of a window, which
            // is the same count `window` takes.
            lines.push(total - 1 - i as u64);
        }
    }
    // Nearest the bottom first, which is the order a search back through the
    // history walks them in.
    lines.reverse();
    Found {
        needle: needle.to_string(),
        lines,
    }
}

fn write_line(line: &Line, out: &mut String) {
    let cells = line.cells();
    // What is actually on the line: everything past this is the blank the
    // screen was built from. Without the trim every line carries the width of
    // the screen in spaces, and a background colour set at the end of one would
    // paint the rest of the row.
    let width = cells
        .iter()
        .rposition(|cell| !cell.is_default())
        .map_or(0, |last| last + 1);
    let default = Pen::default();
    let mut pen = &default;
    for cell in &cells[..width] {
        // The tail half of a wide character, which the head already wrote.
        if cell.width() == 0 {
            continue;
        }
        if cell.pen() != pen {
            out.push_str(&sgr(cell.pen()));
            pen = cell.pen();
        }
        out.push(cell.char());
    }
    if pen != &default {
        out.push_str("\x1b[0m");
    }
    out.push_str("\r\n");
}

/// A pen as one sequence, reset first rather than diffed against the last one.
/// These lines are written once and never read back, so the few extra bytes buy
/// a renderer that cannot drift.
fn sgr(pen: &Pen) -> String {
    let mut sgr = String::from("\x1b[0");
    if pen.is_bold() {
        sgr.push_str(";1");
    }
    if pen.is_faint() {
        sgr.push_str(";2");
    }
    if pen.is_italic() {
        sgr.push_str(";3");
    }
    if pen.is_underline() {
        sgr.push_str(";4");
    }
    if pen.is_blink() {
        sgr.push_str(";5");
    }
    if pen.is_inverse() {
        sgr.push_str(";7");
    }
    if pen.is_strikethrough() {
        sgr.push_str(";9");
    }
    if let Some(color) = pen.foreground() {
        sgr.push_str(&params(color, 3));
    }
    if let Some(color) = pen.background() {
        sgr.push_str(&params(color, 4));
    }
    sgr.push('m');
    sgr
}

/// A colour's parameters. `base` is 3 for a foreground and 4 for a background,
/// which is the only difference between the two in every spelling SGR has: 30
/// and 40 for the first eight, 90 and 100 for their bright halves, 38 and 48
/// for the indexed and direct forms.
fn params(color: Color, base: u8) -> String {
    match color {
        Color::Indexed(i) if i < 8 => format!(";{}", base * 10 + i),
        Color::Indexed(i) if i < 16 => format!(";{}", base * 10 + 52 + i),
        Color::Indexed(i) => format!(";{base}8;5;{i}"),
        Color::RGB(c) => format!(";{base}8;2;{};{};{}", c.r, c.g, c.b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vt(cols: usize, rows: usize) -> Vt {
        Vt::builder().size(cols, rows).scrollback_limit(100).build()
    }

    #[test]
    fn the_history_is_what_the_screen_has_already_scrolled_past() {
        let mut vt = vt(20, 3);
        for i in 1..=6 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        // Six lines and the one the cursor sits on, of which the last three are
        // the screen and the dump's to paint.
        assert_eq!(render(&vt, 10), "line 1\r\nline 2\r\nline 3\r\nline 4\r\n");
    }

    #[test]
    fn only_the_last_lines_asked_for_are_rendered() {
        let mut vt = vt(20, 3);
        for i in 1..=6 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        assert_eq!(render(&vt, 2), "line 3\r\nline 4\r\n");
        assert_eq!(render(&vt, 0), "");
    }

    /// A screen that has never scrolled has nothing behind it, and neither has
    /// the alternate screen: `avt` gives that buffer no scrollback at all, so a
    /// session sitting in vim seeds nothing and the dump does all the work.
    #[test]
    fn a_screen_that_has_not_scrolled_has_no_history() {
        let mut fresh = vt(20, 3);
        fresh.feed_str("hello\r\n");
        assert_eq!(render(&fresh, 10), "");

        let mut full_screen = vt(20, 3);
        for i in 1..=6 {
            full_screen.feed_str(&format!("line {i}\r\n"));
        }
        full_screen.feed_str("\x1b[?1049h");
        assert_eq!(render(&full_screen, 10), "");
    }

    #[test]
    fn colour_survives_the_trip() {
        let mut vt = vt(20, 3);
        vt.feed_str("\x1b[31mred\x1b[0m plain\r\n");
        for i in 1..=4 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        let rendered = render(&vt, 10);
        assert!(
            rendered.starts_with("\x1b[0;31mred\x1b[0m plain\r\n"),
            "{rendered:?}"
        );
    }

    /// The window a scrolling client reads. The screen is part of it: scrolling
    /// up from the bottom has to be continuous, and the bottom is the screen.
    #[test]
    fn a_window_counts_back_from_the_newest_line() {
        let mut vt = vt(20, 3);
        for i in 1..=6 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        // Seven lines: six printed and the one the cursor sits on.
        let view = window(&vt, &ViewRequest { from: 0, lines: 3 });
        assert_eq!(view.total, 7);
        assert_eq!(view.from, 0);
        assert_eq!(view.lines, vec!["line 5", "line 6", ""]);

        // Two lines further back.
        let view = window(&vt, &ViewRequest { from: 2, lines: 3 });
        assert_eq!(view.lines, vec!["line 3", "line 4", "line 5"]);
    }

    /// A client asking past either end gets what is there. The buffer trims
    /// from the top while it is being read, so running off it is ordinary.
    #[test]
    fn a_window_off_the_end_is_clamped_rather_than_refused() {
        let mut vt = vt(20, 3);
        for i in 1..=4 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        let view = window(&vt, &ViewRequest {
            from: 999,
            lines: 3,
        });
        assert_eq!(view.from, 4, "the top of a five line buffer");
        assert_eq!(view.lines, vec!["line 1"]);

        let view = window(&vt, &ViewRequest { from: 0, lines: 99 });
        assert_eq!(view.lines.len(), 5, "everything there is");
    }

    /// Offsets rather than lines, in the same count a window takes, so that
    /// jumping to a match is the ordinary fetch with a number from here.
    #[test]
    fn a_search_answers_with_where_the_matches_are() {
        let mut vt = vt(20, 3);
        for i in 1..=6 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        // Seven lines, the last being the one the cursor sits on. `line 6` is
        // index 5 of 7, so it is one back from the newest.
        let found = find(&vt, "line 6");
        assert_eq!(found.lines, vec![1]);
        assert_eq!(found.needle, "line 6");

        // Nearest the bottom first: that is the order a search back through
        // the history walks them in.
        let found = find(&vt, "line");
        assert_eq!(found.lines, vec![1, 2, 3, 4, 5, 6]);

        assert!(find(&vt, "nothing here").lines.is_empty());
        assert!(
            find(&vt, "").lines.is_empty(),
            "an empty needle is nobody having typed yet, not everything"
        );
    }

    /// Smartcase, the way `less` and vim do it, because a search you type in a
    /// hurry is lowercase and a search you meant is not.
    #[test]
    fn a_lowercase_search_ignores_case_and_a_capital_means_it() {
        let mut vt = vt(20, 3);
        vt.feed_str("Warning: no\r\n");
        for i in 1..=4 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        assert_eq!(find(&vt, "warning").lines.len(), 1);
        assert_eq!(find(&vt, "Warning").lines.len(), 1);
        assert!(find(&vt, "WARNING").lines.is_empty());
    }

    #[test]
    fn trailing_blanks_are_not_written_out() {
        let mut vt = vt(20, 3);
        vt.feed_str("hi\r\n");
        for i in 1..=4 {
            vt.feed_str(&format!("line {i}\r\n"));
        }
        assert!(render(&vt, 10).starts_with("hi\r\n"));
    }
}
