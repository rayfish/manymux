//! The two ways to put a session on a terminal.
//!
//! `alternate` takes the terminal's second screen buffer and paints the session
//! there. It behaves the same everywhere and gives your shell's screen back
//! untouched, and it is the reason the wheel is useless while attached: that
//! buffer has no scrollback, so terminals turn the wheel into arrow keys and
//! they land in whatever is reading the session's input.
//!
//! `inline` leaves the terminal on its own screen. What the session prints
//! scrolls into the terminal's scrollback, which means the wheel, the find bar
//! and selection are the terminal's own and none of them are ours to build. The
//! price is that the terminal has to keep what scrolls out of a scrolling
//! region, because the mark's row is fenced off with one, and that a hop leaves
//! the session you left in the scrollback rather than wiping it.
//!
//! Only what differs between them is here, and all of it is string building
//! with no terminal involved, so both can be read side by side and tested
//! without one. What they have in common (the title, and undoing whatever the
//! last full-screen program switched on) is written by the caller in
//! [`super::attach`].

use crate::client::status::session_size;
use crate::proto::Size;
use crate::settings::Screen;

/// Lines of a session's history an inline client asks the node for.
///
/// Enough to scroll back through a build you were not there for, and small
/// enough that attaching is still one write. The node keeps ten times as many;
/// what is not asked for is not lost, it is just not in this terminal.
pub const SEEDED_HISTORY: u32 = 1000;

/// What one way of putting a session on a terminal has to say for itself.
///
/// Six moments, and the caller writes what both modes have in common around
/// them.
pub trait ScreenMode {
    /// Sent once, when the terminal changes hands.
    fn setup(&self) -> String;

    /// Sent before every attach, hops included, after the caller's `undone()`.
    fn takeover(&self) -> String;

    /// Sent before the screen dump, which paints by absolute coordinates from
    /// the top and so needs the screen underneath it blank.
    fn before_repaint(&self, size: Size) -> String;

    /// Sent when the terminal is given back, after the caller's `undone()` and
    /// the title pop. `on_alternate` is whether the session has the terminal on
    /// a full-screen program's own screen.
    fn reset(&self, size: Size, on_alternate: bool) -> String;

    /// Lines of the session's history to ask the node for.
    fn history(&self) -> u32;

    /// Whether the session's own switches between the primary and alternate
    /// screens are the client's to swallow.
    fn owns_the_screen(&self) -> bool;
}

/// The session gets a screen of its own, and the shell's is given back
/// untouched.
pub struct Alternate;

/// The session paints on the terminal's screen, and its history goes into the
/// terminal's scrollback.
pub struct Inline;

impl Screen {
    pub fn mode(self) -> &'static dyn ScreenMode {
        match self {
            Screen::Alternate => &Alternate,
            Screen::Inline => &Inline,
        }
    }
}

impl ScreenMode for Alternate {
    fn setup(&self) -> String {
        // A session's repaint places everything by absolute coordinates,
        // because that is what a screen dump is, so painting it onto your
        // shell's screen would write the session over your scrollback and leave
        // the cursor at the session's row 1 rather than where its prompt
        // appears to be. On a surface of its own the coordinates mean what they
        // say.
        "\x1b[?1049h".to_string()
    }

    fn takeover(&self) -> String {
        // The erase is what the screen dump cannot do for itself: it paints
        // from the cursor down to its last line with anything on it and stops,
        // so without this the session before it shows through underneath, below
        // that line and beside a narrower screen. Homing is the other half,
        // since the dump starts printing wherever it finds the cursor. The
        // erase comes last because it clears to the current background, and a
        // program that left a pen set would otherwise paint the whole screen
        // its colour.
        "\x1b[H\x1b[2J".to_string()
    }

    fn before_repaint(&self, _size: Size) -> String {
        String::new()
    }

    fn reset(&self, _size: Size, _on_alternate: bool) -> String {
        // Column zero, but no newline: leaving the alternate screen already put
        // the cursor back where the shell left it, on the line after the
        // command you typed. A newline here would print the detach message one
        // blank line further down for no reason.
        "\x1b[?1047l\x1b[?1049l\r".to_string()
    }

    fn history(&self) -> u32 {
        0
    }

    fn owns_the_screen(&self) -> bool {
        true
    }
}

impl ScreenMode for Inline {
    fn setup(&self) -> String {
        String::new()
    }

    fn takeover(&self) -> String {
        // Nothing is erased: what is on the screen is the terminal's to keep,
        // and rolling it away before the repaint is what moves it into the
        // scrollback instead of throwing it out.
        String::new()
    }

    fn before_repaint(&self, size: Size) -> String {
        // A newline on the last row of the scrolling region scrolls the region,
        // and what scrolls off the top of it is what the terminal keeps. So
        // this leaves the same blank screen an erase would, with every line
        // still reachable by the wheel. It runs here rather than in the
        // takeover because the history the node sends goes out first and has to
        // be scrolled away with everything else.
        let rows = session_size(size).rows;
        format!("\x1b[{rows};1H{}\x1b[H", "\n".repeat(usize::from(rows)))
    }

    fn reset(&self, size: Size, on_alternate: bool) -> String {
        let mut reset = String::new();
        if on_alternate {
            // The session is in a full-screen program and the terminal is on
            // its screen, so without this the shell would come back to vim's
            // leftovers.
            reset.push_str("\x1b[?1047l\x1b[?1049l");
        }
        // What the session last painted stays where a finished program's output
        // stays. The shell carries on from the row the mark was on, under it
        // rather than over it.
        format!("{reset}\x1b[{};1H\x1b[2K\r", size.rows)
    }

    fn history(&self) -> u32 {
        SEEDED_HISTORY
    }

    fn owns_the_screen(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZE: Size = Size { cols: 80, rows: 25 };

    #[test]
    fn the_alternate_screen_takes_a_surface_of_its_own_and_erases_it() {
        let mode = Screen::Alternate.mode();
        assert_eq!(mode.setup(), "\x1b[?1049h");
        assert_eq!(mode.takeover(), "\x1b[H\x1b[2J");
        // The takeover already erased, so there is nothing to do before the
        // dump.
        assert_eq!(mode.before_repaint(SIZE), "");
        assert!(mode.owns_the_screen());
        assert_eq!(mode.history(), 0, "there is nowhere to put it");
    }

    #[test]
    fn leaving_the_alternate_screen_hands_back_the_one_underneath() {
        let reset = Screen::Alternate.mode().reset(SIZE, false);
        assert!(reset.contains("\x1b[?1047l"), "{reset:?}");
        assert!(reset.contains("\x1b[?1049l"), "{reset:?}");
        assert!(reset.ends_with('\r'), "{reset:?}");
    }

    #[test]
    fn inline_takes_no_screen_and_erases_nothing() {
        let mode = Screen::Inline.mode();
        assert_eq!(mode.setup(), "");
        assert_eq!(mode.takeover(), "");
        assert!(!mode.owns_the_screen());
        assert_eq!(mode.history(), SEEDED_HISTORY);
    }

    /// The roll is what puts a screenful into the terminal's scrollback:
    /// newlines on the last row of the region scroll it, and what scrolls off
    /// the top is what the terminal keeps. An erase would throw the same lines
    /// away, which is what the alternate screen does to them.
    #[test]
    fn inline_rolls_the_region_away_and_comes_back_to_the_top() {
        let rolled = Screen::Inline.mode().before_repaint(SIZE);
        // 24 rows for the session, one for the mark.
        assert!(rolled.starts_with("\x1b[24;1H"), "{rolled:?}");
        assert_eq!(rolled.matches('\n').count(), 24);
        assert!(rolled.ends_with("\x1b[H"), "{rolled:?}");
        assert!(!rolled.contains("2J"), "erasing is what we are avoiding");
    }

    /// A terminal too short for the mark gives the session every row, so the
    /// region is the whole screen and the roll has to be too.
    #[test]
    fn an_unmarked_terminal_rolls_every_row() {
        let rolled = Screen::Inline
            .mode()
            .before_repaint(Size { cols: 80, rows: 4 });
        assert!(rolled.starts_with("\x1b[4;1H"), "{rolled:?}");
        assert_eq!(rolled.matches('\n').count(), 4);
    }

    /// Detaching from inside a full-screen program has to hand back the screen
    /// the shell is on. Detaching from an ordinary one must not pop a screen
    /// nobody pushed, and leaves the session's last words where a finished
    /// program's output stays.
    #[test]
    fn inline_pops_a_screen_only_when_the_session_is_in_one() {
        let popped = Screen::Inline.mode().reset(SIZE, true);
        assert!(popped.contains("\x1b[?1049l"), "{popped:?}");

        let plain = Screen::Inline.mode().reset(SIZE, false);
        assert!(!plain.contains("1049"), "{plain:?}");
        // The row the mark was on, cleared, for the shell to carry on from.
        assert!(plain.ends_with("\x1b[25;1H\x1b[2K\r"), "{plain:?}");
    }
}
