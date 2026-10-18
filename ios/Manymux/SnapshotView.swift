import SwiftUI
import UIKit

/// One session's screen, drawn small.
///
/// The session's own grid scaled to fit, rather than a screen asked for in the
/// tile's shape: a dump paints by absolute position and never reflows, so a
/// guessed shape would take the picture apart rather than make it smaller.
final class SnapshotUIView: UIView {
    var preview: Preview? {
        didSet {
            rows = Dictionary(
                (preview?.lines ?? []).map { (Int($0.at), $0.runs) },
                uniquingKeysWith: { _, last in last }
            )
            setNeedsDisplay()
        }
    }

    private var rows: [Int: [Run]] = [:]

    /// A cell is half as wide as a line is tall, and the glyph is 60% of a cell
    /// height for a monospaced face: the same ratios as the Android tile.
    private let aspect: CGFloat = 0.5
    private let width: CGFloat = 0.6
    private let floor: CGFloat = 2.5

    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = true
        contentMode = .redraw
        backgroundColor = UIColor(rgb: Palette.ground)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not built from a storyboard")
    }

    override func draw(_ rect: CGRect) {
        guard let context = UIGraphicsGetCurrentContext() else { return }
        context.setFillColor(UIColor(rgb: Palette.ground).cgColor)
        context.fill(bounds)
        guard let screen = preview, screen.cols > 0, screen.rows > 0 else { return }
        let cols = CGFloat(screen.cols)
        let lines = CGFloat(screen.rows)
        // The whole grid into the room there is, whichever way it runs out
        // first, from one number so the cell and the line agree.
        let cell = max(min(bounds.width / cols, bounds.height / lines * aspect), floor)
        let line = cell / aspect
        let painter = Painter(cell: cell, line: line, size: cell / width)
        // What did not fit is taken off the top rather than the bottom.
        let lift = max(lines * line - bounds.height, 0)
        painter.paint(rows, in: context, bounds: bounds, lift: lift, blanks: false)
    }
}

struct SnapshotView: UIViewRepresentable {
    let preview: Preview?

    func makeUIView(context: Context) -> SnapshotUIView {
        SnapshotUIView()
    }

    func updateUIView(_ view: SnapshotUIView, context: Context) {
        if view.preview != preview { view.preview = preview }
    }
}
