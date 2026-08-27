//! What a selected cell is painted with, over a cell the program already
//! coloured.
//!
//! A selection marks text; it does not repaint it. So the band is a background
//! and nothing else: the colour the line was written in, its bold, its
//! underline and its italics all go on saying what they said, and the cells
//! under the hand differ from the cells beside them by the ground they sit on.
//! Which is what every terminal worth copying out of does, and what this did
//! not: one fixed pair for every selected cell turned a coloured listing into a
//! grey slab, so a selection was the one gesture that destroyed the thing it
//! was pointing at.
//!
//! The one thing it cannot leave alone is a foreground the band would swallow.
//! Nothing here can ask the terminal what its palette is, but a colour named in
//! the line is a colour we can measure: the 240 above the theme's own sixteen
//! are fixed by the xterm cube, a direct colour says its own channels, and the
//! first sixteen are read as xterm's standard values, which is a guess at a
//! theme rather than knowledge of one and is why [`READABLE`] is a floor with
//! room under it. A glyph that clears the floor keeps its colour. One that does
//! not is drawn in [`FALLBACK`], which is where every glyph used to end up.
//!
//! Reversed text is the exception that has to be handled rather than kept. A
//! cell drawn reversed is a swap of the two colours it already has, so left
//! alone it would swap the band away and paint the selection out at exactly the
//! place somebody was looking: `pi` draws the message you typed that way. So
//! the swap is undone here and the background it wanted is read as the colour
//! it meant the glyph to be.
//!
//! Terminal-free like the rest of the client's drawing: this answers with a
//! string, and the tests read it.

/// The ground a selected cell sits on: xterm 17, a deep navy.
///
/// Shared with [`super::picker`], whose cursor sits on the same ground: both
/// are one gesture saying "this one".
///
/// Fixed by the cube rather than the theme, so it is the same colour on every
/// terminal and can be measured against. Dark enough that the ordinary run of
/// terminal colours reads on it, and blue enough to be a band rather than a
/// shadow on a screen that is already dark.
pub(super) const BAND: u8 = 17;

/// What a glyph is drawn in when its own colour cannot be read on the band.
const FALLBACK: u8 = 255;

/// The least contrast a colour may have with the band and still be kept.
///
/// WCAG's floor for large text. Terminal text is small, which argues for more,
/// and the first sixteen colours are a guess at the theme, which argues for
/// less: a threshold set high enough to be sure would throw away most of what
/// it was written to keep.
const READABLE: f64 = 3.0;

/// The sequence a selected cell is painted with, over a cell drawn with `pen`.
///
/// `pen` is every sequence the line set before this cell, in order, which is
/// what `scroll::highlighted` accumulates as it walks a line.
pub fn selected(pen: &str) -> String {
    let pen = read(pen);
    let mut out = String::from("\x1b[0");
    for (code, on) in pen.attributes() {
        if on {
            out.push_str(&format!(";{code}"));
        }
    }
    out.push_str(&format!(";48;5;{BAND}"));
    // Reversed, the background is what the glyph is drawn in.
    let glyph = if pen.inverse { pen.bg } else { pen.fg };
    match glyph.filter(|colour| readable(*colour)) {
        Some(colour) => out.push_str(&params(colour)),
        None => out.push_str(&format!(";38;5;{FALLBACK}")),
    }
    out.push('m');
    out
}

/// A colour, as a line spells one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Colour {
    /// One of the 256, the first sixteen of which are the theme's own.
    Indexed(u8),
    /// A colour that says its own channels.
    Direct(u8, u8, u8),
}

/// The parts of a pen a band has to know about.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Pen {
    fg: Option<Colour>,
    bg: Option<Colour>,
    bold: bool,
    faint: bool,
    italic: bool,
    underline: bool,
    blink: bool,
    inverse: bool,
    strike: bool,
}

impl Pen {
    /// The attributes that go back out, and what each is spelt as. Inverse is
    /// not among them: it is read and undone rather than passed on.
    fn attributes(&self) -> [(u8, bool); 6] {
        [
            (1, self.bold),
            (2, self.faint),
            (3, self.italic),
            (4, self.underline),
            (5, self.blink),
            (9, self.strike),
        ]
    }
}

/// The pen a run of sequences leaves behind.
///
/// Whatever is not an SGR sequence is not a pen and is skipped: a line carries
/// only these, but the caller hands over what it walked past rather than what
/// it understood.
fn read(seqs: &str) -> Pen {
    let mut pen = Pen::default();
    for body in sgr(seqs) {
        apply(&mut pen, body);
    }
    pen
}

/// The bodies of the SGR sequences in a string, in order: what sits between
/// `CSI` and the `m` that ends it.
fn sgr(seqs: &str) -> impl Iterator<Item = &str> {
    seqs.split('\x1b').filter_map(|piece| {
        let rest = piece.strip_prefix('[')?;
        let end = rest.find(|c: char| ('\x40'..='\x7e').contains(&c))?;
        (rest.as_bytes()[end] == b'm').then(|| &rest[..end])
    })
}

/// One SGR body, applied to the pen it changes.
fn apply(pen: &mut Pen, body: &str) {
    // An empty body is `CSI m`, which is a reset, and so is every parameter
    // nobody wrote: `0` is what an empty one means.
    let mut params = body.split(';').map(|p| p.parse::<u16>().unwrap_or(0));
    while let Some(param) = params.next() {
        match param {
            0 => *pen = Pen::default(),
            1 => pen.bold = true,
            2 => pen.faint = true,
            3 => pen.italic = true,
            4 => pen.underline = true,
            5 => pen.blink = true,
            7 => pen.inverse = true,
            9 => pen.strike = true,
            21 | 22 => {
                pen.bold = false;
                pen.faint = false;
            }
            23 => pen.italic = false,
            24 => pen.underline = false,
            25 => pen.blink = false,
            27 => pen.inverse = false,
            29 => pen.strike = false,
            30..=37 => pen.fg = indexed(param - 30),
            38 => pen.fg = colour(&mut params),
            39 => pen.fg = None,
            40..=47 => pen.bg = indexed(param - 40),
            48 => pen.bg = colour(&mut params),
            49 => pen.bg = None,
            90..=97 => pen.fg = indexed(param - 90 + 8),
            100..=107 => pen.bg = indexed(param - 100 + 8),
            _ => {}
        }
    }
}

fn indexed(at: u16) -> Option<Colour> {
    u8::try_from(at).ok().map(Colour::Indexed)
}

/// The colour after a `38` or a `48`, in either of the two spellings that can
/// follow one.
fn colour(params: &mut impl Iterator<Item = u16>) -> Option<Colour> {
    match params.next()? {
        5 => indexed(params.next()?),
        2 => {
            let mut channel = || u8::try_from(params.next()?).ok();
            Some(Colour::Direct(channel()?, channel()?, channel()?))
        }
        _ => None,
    }
}

/// A colour as the parameters that set it as a foreground.
///
/// Always the long spelling, which every one of the 256 has and which needs no
/// case for where the theme's own sixteen stop.
fn params(colour: Colour) -> String {
    match colour {
        Colour::Indexed(at) => format!(";38;5;{at}"),
        Colour::Direct(r, g, b) => format!(";38;2;{r};{g};{b}"),
    }
}

/// Whether a glyph in this colour can be read on the band.
fn readable(colour: Colour) -> bool {
    contrast(
        luminance(rgb(colour)),
        luminance(rgb(Colour::Indexed(BAND))),
    ) >= READABLE
}

/// What a colour actually is, in channels.
///
/// The cube and the grey ramp above it are the same on every terminal. The
/// first sixteen are the theme's to choose and xterm's are used as a stand-in,
/// which is the guess [`READABLE`] leaves room for.
fn rgb(colour: Colour) -> (u8, u8, u8) {
    /// xterm's own, for the sixteen a theme may spell differently.
    const THEME: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 0, 0),
        (0, 205, 0),
        (205, 205, 0),
        (0, 0, 238),
        (205, 0, 205),
        (0, 205, 205),
        (229, 229, 229),
        (127, 127, 127),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (92, 92, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    /// The six levels a channel of the cube takes.
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match colour {
        Colour::Direct(r, g, b) => (r, g, b),
        Colour::Indexed(at @ 0..=15) => THEME[usize::from(at)],
        Colour::Indexed(at @ 16..=231) => {
            let at = usize::from(at) - 16;
            (LEVELS[at / 36], LEVELS[at / 6 % 6], LEVELS[at % 6])
        }
        // The grey ramp: 24 steps of ten, starting at eight.
        Colour::Indexed(at) => {
            let grey = 8 + (at - 232) * 10;
            (grey, grey, grey)
        }
    }
}

/// Relative luminance, as WCAG defines it.
fn luminance((r, g, b): (u8, u8, u8)) -> f64 {
    fn channel(value: u8) -> f64 {
        let value = f64::from(value) / 255.0;
        if value <= 0.03928 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

/// The contrast ratio between two luminances, which runs from 1 to 21.
fn contrast(one: f64, other: f64) -> f64 {
    (one.max(other) + 0.05) / (one.min(other) + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The band, spelt the way it goes on the wire.
    const BAND: &str = "48;5;17";

    /// What a glyph falls back to when its own colour cannot be read on the
    /// band.
    const FALLBACK: &str = "38;5;255";

    #[test]
    fn a_colour_that_reads_on_the_band_keeps_it() {
        // Cube green, bright enough over a deep navy to stay itself.
        let painted = selected("\x1b[0;38;5;46m");
        assert!(painted.contains(BAND), "{painted:?}");
        assert!(painted.contains("38;5;46"), "{painted:?}");
    }

    #[test]
    fn a_colour_too_dark_for_the_band_gives_way_to_the_fallback() {
        // Blue on blue: the one thing the band cannot be drawn under.
        let painted = selected("\x1b[0;34m");
        assert!(painted.contains(FALLBACK), "{painted:?}");
        assert!(!painted.contains("38;5;4m"), "{painted:?}");
    }

    #[test]
    fn a_cell_with_no_colour_of_its_own_takes_the_fallback() {
        assert_eq!(selected(""), format!("\x1b[0;{BAND};{FALLBACK}m"));
    }

    #[test]
    fn a_reversed_cell_is_painted_the_colour_it_meant_the_glyph_to_be() {
        // Reversed, the background is what the glyph is drawn in. Left as it
        // arrived it would swap the band away and take the selection with it.
        assert_eq!(
            selected("\x1b[0;7;48;5;46;38;5;16m"),
            format!("\x1b[0;{BAND};38;5;46m")
        );
    }

    #[test]
    fn a_reversed_cell_with_no_colours_of_its_own_takes_the_fallback() {
        // The glyph would be the terminal's own background, which is a colour
        // nothing here knows and which the band would swallow.
        let painted = selected("\x1b[0;7m");
        assert!(painted.contains(FALLBACK), "{painted:?}");
    }

    #[test]
    fn the_shape_of_the_text_survives_the_band() {
        assert_eq!(
            selected("\x1b[0;1;3;4;9;38;5;46m"),
            format!("\x1b[0;1;3;4;9;{BAND};38;5;46m")
        );
    }

    #[test]
    fn a_direct_colour_is_measured_like_an_indexed_one() {
        let painted = selected("\x1b[0;38;2;255;255;0m");
        assert!(painted.contains("38;2;255;255;0"), "{painted:?}");
    }

    #[test]
    fn the_pen_the_cell_ended_with_is_the_one_that_counts() {
        // Two whole pens, as a line hands them over: the second is what the
        // cell is wearing, and it is too dark for the band.
        let painted = selected("\x1b[0;38;5;46m\x1b[0;34m");
        assert!(painted.contains(FALLBACK), "{painted:?}");
        assert!(!painted.contains("38;5;46"), "{painted:?}");
    }

    #[test]
    fn an_attribute_turned_off_again_is_not_painted_back_on() {
        assert_eq!(selected("\x1b[0;1m\x1b[22m"), selected(""));
    }

    #[test]
    fn a_sequence_that_is_not_a_pen_is_not_read_as_one() {
        // A cursor move carries a `7`, which is the inverse parameter in an
        // SGR and nothing at all in this.
        assert_eq!(selected("\x1b[7;1H"), selected(""));
    }

    #[test]
    fn the_grey_ramp_and_the_cube_are_read_where_they_actually_are() {
        assert_eq!(rgb(Colour::Indexed(17)), (0, 0, 95));
        assert_eq!(rgb(Colour::Indexed(46)), (0, 255, 0));
        assert_eq!(rgb(Colour::Indexed(231)), (255, 255, 255));
        assert_eq!(rgb(Colour::Indexed(232)), (8, 8, 8));
        assert_eq!(rgb(Colour::Indexed(255)), (238, 238, 238));
    }

    /// The point of the floor: what a person actually has on the screen is
    /// mostly the theme's sixteen, and most of them have to survive or the
    /// band is the grey slab again by another route.
    #[test]
    fn the_colours_a_terminal_actually_uses_mostly_survive_the_band() {
        let kept = (0..16).filter(|at| readable(Colour::Indexed(*at))).count();
        assert!(kept >= 11, "only {kept} of the sixteen survive");
    }
}
