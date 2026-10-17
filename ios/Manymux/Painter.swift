import UIKit

/// Draws rows of runs onto a grid, for the session and for its thumbnail.
///
/// One of these rather than two copies, so a tile and a session cannot disagree
/// about what a run looks like. A run is advanced by its own cell count and
/// never by the font's idea of how wide the text is: a wide character is two
/// cells and one glyph, and measuring the glyph puts the rest of the row out of
/// step with the grid.
struct Painter {
    let regular: UIFont
    let bold: UIFont
    let cell: CGFloat
    let line: CGFloat
    /// How far down the line the text's own box starts, so it sits in the
    /// middle of a line that may be taller than the font.
    let inset: CGFloat

    /// A painter whose grid is the font's own.
    init(size: CGFloat) {
        let font = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        self.init(
            cell: ("M" as NSString).size(withAttributes: [.font: font]).width,
            line: font.lineHeight.rounded(.up),
            size: size
        )
    }

    /// A painter fitted to a grid it was handed, as a thumbnail is.
    init(cell: CGFloat, line: CGFloat, size: CGFloat) {
        regular = UIFont.monospacedSystemFont(ofSize: size, weight: .regular)
        bold = UIFont.monospacedSystemFont(ofSize: size, weight: .bold)
        self.cell = cell
        self.line = line
        inset = (line - regular.lineHeight) / 2
    }

    /// Paint each row at its own index, `lift` points higher than that.
    ///
    /// Blank runs are background and nothing else, so their glyphs are skipped
    /// when `blanks` is false: a tile drawing every space is a tile paying for
    /// a screenful of nothing.
    func paint(
        _ rows: [Int: [Run]],
        in context: CGContext,
        bounds: CGRect,
        lift: CGFloat = 0,
        blanks: Bool = true
    ) {
        for (at, runs) in rows {
            let top = CGFloat(at) * line - lift
            if top > bounds.height || top + line < 0 { continue }
            var x: CGFloat = 0
            for run in runs {
                if x > bounds.width { break }
                let across = CGFloat(run.cells) * cell
                let background = Palette.of(run.look.background, or: Palette.ground)
                let foreground = Palette.of(run.look.foreground, or: Palette.text)
                let paper = run.look.inverse ? foreground : background
                let ink = run.look.inverse ? background : foreground
                if paper != Palette.ground {
                    context.setFillColor(UIColor(rgb: paper).cgColor)
                    context.fill(CGRect(x: x, y: top, width: across, height: line))
                }
                if blanks || !run.text.allSatisfy(\.isWhitespace) {
                    draw(run, colour: ink, at: CGPoint(x: x, y: top + inset))
                }
                x += across
            }
        }
    }

    private func draw(_ run: Run, colour: UInt32, at point: CGPoint) {
        var attributes: [NSAttributedString.Key: Any] = [
            .font: run.look.bold ? bold : regular,
            .foregroundColor: UIColor(rgb: colour, alpha: run.look.faint ? 0.6 : 1),
        ]
        if run.look.underline {
            attributes[.underlineStyle] = NSUnderlineStyle.single.rawValue
        }
        if run.look.strikethrough {
            attributes[.strikethroughStyle] = NSUnderlineStyle.single.rawValue
        }
        if run.look.italic {
            attributes[.obliqueness] = 0.2
        }
        (run.text as NSString).draw(at: point, withAttributes: attributes)
    }
}
