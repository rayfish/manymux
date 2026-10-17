import UIKit

/// How tall one cell's text is, in points. The Android app's 12.5dp, which is
/// about fifty columns on an ordinary phone.
private let cellPoints: CGFloat = 12.5

/// What is left of a fling's speed after a sixtieth of a second, spent against
/// the frame's own length so a 120Hz screen coasts as far as a 60Hz one.
private let decay: CGFloat = 0.92

/// The session's screen.
///
/// A plain `UIView` with a `draw(_:)`, for the reason the Android app uses a
/// plain `View`: the grid has no structure to lay out, it is one surface
/// changing as fast as the session prints. Rows are pulled once a frame from a
/// display link rather than pushed per byte, so output arriving faster than the
/// screen is drawn coalesces in the emulator on the other side of the boundary
/// and a frame with nothing in it is skipped.
///
/// It is the keyboard's first responder through `UIKeyInput` and nothing more:
/// a terminal is not a text field, so there is no marked text, no
/// autocorrection and no selection for the keyboard to manage.
final class TerminalView: UIView, UIKeyInput {
    /// What the session is attached through, once there is one.
    var attach: Attach? {
        didSet {
            endSelection()
            rows.removeAll()
            history.removeAll()
            viewing = false
            viewOpen = false
            looking = false
            stopFling()
            said = false
            held.control = false
            held.shift = false
            cursorOn = false
            viewFrom = 0
            viewTotal = 0
            screenRows = Int.max
            // A fresh attach has been told nothing, whatever the last one knew.
            told = nil
            tellGrid()
            setNeedsDisplay()
        }
    }

    /// The modifiers the extra keys hold, shared with the row that shows them.
    let held: Held

    /// Told the shape of the grid whenever it changes.
    var onGrid: ((Grid) -> Void)?

    /// Told once per attach when somebody scrolls on a host that cannot answer
    /// for it: a gesture that quietly does nothing is worse than a key that
    /// does, there being nothing on the screen saying it was ever a gesture.
    var onCannotScroll: (() -> Void)?

    private let painter = Painter(size: cellPoints)
    private var rows: [Int: [Run]] = [:]
    private var cursorCol = 0
    private var cursorRow = 0
    private var cursorOn = false

    /// The view over the host's history, kept beside the screen's rows: the
    /// session goes on running while somebody reads what it printed a minute
    /// ago, so coming back is a redraw and not a round trip.
    private var history: [Int: [Run]] = [:]
    private var viewing = false
    private var viewOpen = false
    private var viewFrom: UInt64 = 0
    private var viewTotal: UInt64 = 0
    private var looking = false
    private var said = false

    /// The last grid the far end was told about.
    private var told: Grid?
    /// How many rows the last frame said the screen had.
    private var screenRows = Int.max

    private var link: CADisplayLink?
    private var lastFrame: CFTimeInterval = 0

    /// Points dragged that have not yet added up to a whole line.
    private var pending: CGFloat = 0
    /// A fling's speed in points a second, or zero while nothing coasts.
    private var speed: CGFloat = 0
    /// The cell the hand was last in, which is where a notch is reported.
    private var pointer = At(col: 0, row: 0)

    private var selection: [Int: [Run]]?
    private var selectionStart = At(col: 0, row: 0)
    private var selectionEnd = At(col: 0, row: 0)
    private lazy var menu = UIEditMenuInteraction(delegate: self)

    init(held: Held) {
        self.held = held
        super.init(frame: .zero)
        backgroundColor = UIColor(rgb: Palette.ground)
        isOpaque = true
        contentMode = .redraw
        isMultipleTouchEnabled = false

        let tap = UITapGestureRecognizer(target: self, action: #selector(tapped))
        let pan = UIPanGestureRecognizer(target: self, action: #selector(panned))
        pan.maximumNumberOfTouches = 1
        let press = UILongPressGestureRecognizer(target: self, action: #selector(pressed))
        addGestureRecognizer(tap)
        addGestureRecognizer(pan)
        addGestureRecognizer(press)

        addInteraction(menu)

        isAccessibilityElement = true
        accessibilityLabel = "terminal"
        accessibilityTraits = .allowsDirectInteraction
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("not built from a storyboard")
    }

    // ---- the grid -------------------------------------------------------

    override func layoutSubviews() {
        super.layoutSubviews()
        tellGrid()
        if wanted, bounds.width > 0, bounds.height > 0 {
            wanted = false
            becomeFirstResponder()
        }
    }

    /// How many cells fit, which is what the far end is told.
    private func grid() -> Grid {
        let across = Int(bounds.width / painter.cell)
        let down = Int(bounds.height / painter.line)
        return Grid(cols: UInt16(clamping: max(across, 1)), rows: UInt16(clamping: max(down, 1)))
    }

    /// Tell the far end the shape, if it is not the shape it was already told.
    private func tellGrid() {
        guard bounds.width > 0, bounds.height > 0 else { return }
        let grid = grid()
        if grid == told { return }
        endSelection()
        told = grid
        forget(below: Int(grid.rows))
        attach?.resize(grid: grid)
        onGrid?(grid)
    }

    /// Drop what is held for rows the screen no longer has. A frame carries the
    /// rows that changed and never mentions the ones a shorter screen lost, so
    /// without this the old bottom line goes on being drawn under the new one.
    private func forget(below down: Int) {
        defer { screenRows = down }
        guard down < screenRows else { return }
        rows = rows.filter { $0.key < down }
        history = history.filter { $0.key < down }
    }

    // ---- the frame clock -----------------------------------------------

    override func didMoveToWindow() {
        super.didMoveToWindow()
        link?.invalidate()
        link = nil
        guard window != nil else { return }
        // A proxy holds the view weakly, the display link holding its target
        // strongly and the view holding the link.
        let link = CADisplayLink(target: Tick(self), selector: #selector(Tick.fire))
        link.add(to: .main, forMode: .common)
        self.link = link
    }

    /// Once a frame: take whatever changed and draw only if something did.
    fileprivate func frame(_ now: CFTimeInterval) {
        defer { lastFrame = now }
        guard let session = attach else { return }
        coast(now)
        var redraw = false
        let frame = session.takeFrame()
        let moved = Int(frame.cursor.col) != cursorCol
            || Int(frame.cursor.row) != cursorRow
            || frame.cursor.visible != cursorOn
        if !frame.changed.isEmpty || moved {
            forget(below: Int(frame.rows))
            for row in frame.changed {
                rows[Int(row.at)] = row.runs
            }
            cursorCol = Int(frame.cursor.col)
            cursorRow = Int(frame.cursor.row)
            cursorOn = frame.cursor.visible
            // The session still paints behind an open view, and must not
            // paint over what somebody is reading.
            redraw = redraw || !viewing
        }
        // Asked for only once somebody has reached for the history: a session
        // nobody scrolls should not pay a call across the boundary every frame
        // to be told so.
        if looking || viewOpen {
            let window = session.takeWindow()
            // A view opened again is opened at the bottom, so the rows the last
            // one left behind say nothing about this one.
            if window.open && !viewOpen { history.removeAll() }
            viewOpen = window.open
            if !window.open { looking = false }
            viewFrom = window.from
            viewTotal = window.total
            for row in window.changed {
                history[Int(row.at)] = row.runs
            }
            let showing = window.open && window.showing
            redraw = redraw || !window.changed.isEmpty || showing != viewing
            viewing = showing
        }
        if redraw { setNeedsDisplay() }
    }

    // ---- drawing --------------------------------------------------------

    override func draw(_ rect: CGRect) {
        guard let context = UIGraphicsGetCurrentContext() else { return }
        context.setFillColor(UIColor(rgb: Palette.ground).cgColor)
        context.fill(bounds)
        painter.paint(selection ?? (viewing ? history : rows), in: context, bounds: bounds)
        if selection != nil {
            drawSelection(context)
            return
        }
        if viewing {
            drawMark(context)
            return
        }
        if cursorOn {
            context.setFillColor(UIColor(rgb: Palette.text, alpha: 0.53).cgColor)
            context.fill(CGRect(
                x: CGFloat(cursorCol) * painter.cell,
                y: CGFloat(cursorRow) * painter.line,
                width: painter.cell,
                height: painter.line
            ))
        }
    }

    /// Where in the history the view is, down the right-hand edge. A screen of
    /// old output looks exactly like a screen of new output, and without this
    /// the only sign of the difference is that nothing is moving.
    private func drawMark(_ context: CGContext) {
        guard viewTotal > 0 else { return }
        let shown = max(Int(bounds.height / painter.line), 1)
        let reach = max(Int64(viewTotal) - Int64(shown), 1)
        let from = min(max(Int64(viewFrom), 0), reach)
        let down = 1 - CGFloat(from) / CGFloat(reach)
        let bar: CGFloat = 3
        let tall = max(bounds.height * CGFloat(shown) / CGFloat(viewTotal), 24)
        let top = (bounds.height - tall) * down
        context.setFillColor(UIColor(rgb: Palette.text, alpha: 0.4).cgColor)
        context.fill(CGRect(x: bounds.width - bar * 2, y: top, width: bar, height: tall))
    }

    private func drawSelection(_ context: CGContext) {
        guard let grid = told else { return }
        let cols = Int(grid.cols)
        let a = Int(selectionStart.row) * cols + Int(selectionStart.col)
        let b = Int(selectionEnd.row) * cols + Int(selectionEnd.col)
        let first = min(a, b)
        let last = max(a, b)
        context.setFillColor(UIColor(rgb: Palette.text, alpha: 0.33).cgColor)
        for row in (first / cols)...(last / cols) {
            let left = row == first / cols ? first % cols : 0
            let right = row == last / cols ? last % cols + 1 : cols
            context.fill(CGRect(
                x: CGFloat(left) * painter.cell,
                y: CGFloat(row) * painter.line,
                width: CGFloat(right - left) * painter.cell,
                height: painter.line
            ))
        }
    }

    // ---- touch ----------------------------------------------------------

    /// A tap while the history is up is the way back to the session; otherwise
    /// it is the way back to the keyboard, a terminal having no field to tap.
    @objc private func tapped() {
        if selection != nil {
            endSelection()
        } else if viewOpen {
            leaveHistory()
        } else {
            openKeyboard()
        }
    }

    @objc private func panned(_ pan: UIPanGestureRecognizer) {
        let at = cell(under: pan.location(in: self))
        if selection != nil {
            select(pan, at: at)
            return
        }
        switch pan.state {
        case .began:
            // A hand on the screen stops a fling.
            stopFling()
            pointer = at
        case .changed:
            pointer = at
            drag(pan.translation(in: self).y)
            pan.setTranslation(.zero, in: self)
        case .ended:
            speed = pan.velocity(in: self).y
            if abs(speed) < painter.line * 2 {
                speed = 0
                settle()
            }
        case .cancelled, .failed:
            speed = 0
            settle()
        default:
            break
        }
    }

    /// Long press freezes the picture; dragging selects cells until Copy.
    @objc private func pressed(_ press: UILongPressGestureRecognizer) {
        if press.state == .began {
            stopFling()
            selection = viewing ? history : rows
        }
        select(press, at: cell(under: press.location(in: self)))
    }

    /// Either gesture moving the far end of a selection.
    private func select(_ gesture: UIGestureRecognizer, at: At) {
        switch gesture.state {
        case .began:
            selectionStart = at
            selectionEnd = at
        case .changed:
            selectionEnd = at
        case .ended:
            selectionEnd = at
            showMenu(at: gesture.location(in: self))
        default:
            break
        }
        setNeedsDisplay()
    }

    private func stopFling() {
        speed = 0
        pending = 0
    }

    /// Move by however many whole lines this much dragging comes to, keeping
    /// the remainder: a slow drag is a run of fractions of a line, and
    /// rounding each to nothing is a screen that only moves for a fast hand.
    private func drag(_ points: CGFloat) {
        guard let session = attach, painter.line > 0 else { return }
        pending += points
        let lines = Int((pending / painter.line).rounded(.towardZero))
        if lines == 0 { return }
        pending -= CGFloat(lines) * painter.line
        // Which of the two things a drag means is the far end's to decide: it
        // knows whether the program in there is reading the mouse, and how
        // that program asked to be told.
        switch session.drag(lines: Int64(lines), at: pointer) {
        case .wheeled:
            break
        case .looked:
            if lines > 0 { looking = true }
        case .nowhere:
            if !said {
                said = true
                onCannotScroll?()
            }
        }
    }

    /// Carry a fling on, and stop it where there is nothing left to move to.
    private func coast(_ now: CFTimeInterval) {
        guard speed != 0 else { return }
        let seconds = lastFrame == 0 ? 0 : CGFloat(now - lastFrame)
        guard seconds > 0, seconds <= 0.25 else { return }
        drag(speed * seconds)
        speed *= pow(decay, seconds * 60)
        let shown = UInt64(max(bounds.height / painter.line, 0))
        let ends = (viewFrom == 0 && speed < 0)
            || (viewTotal > 0 && viewFrom + shown >= viewTotal && speed > 0)
        if abs(speed) < max(painter.line * 2, 1) || ends {
            stopFling()
            settle()
        }
    }

    /// A gesture that ended at the live screen leaves the view behind.
    private func settle() {
        if viewOpen && viewFrom == 0 { attach?.closeView() }
    }

    /// Which cell a touch landed in, clamped to the grid the far end was told
    /// about: a report naming a row the session has not got is one it reads as
    /// being somewhere else entirely.
    private func cell(under point: CGPoint) -> At {
        guard let grid = told else { return At(col: 0, row: 0) }
        let col = min(max(Int(point.x / painter.cell), 0), max(Int(grid.cols) - 1, 0))
        let row = min(max(Int(point.y / painter.line), 0), max(Int(grid.rows) - 1, 0))
        return At(col: UInt16(col), row: UInt16(row))
    }

    // ---- selection ------------------------------------------------------

    private func showMenu(at point: CGPoint) {
        menu.presentEditMenu(with: UIEditMenuConfiguration(identifier: nil, sourcePoint: point))
    }

    private func endSelection() {
        guard selection != nil else { return }
        selection = nil
        menu.dismissMenu()
        setNeedsDisplay()
    }

    fileprivate func copySelection() {
        guard let frozen = selection else { return }
        let picked = frozen.map { Row(at: UInt16($0.key), runs: $0.value) }
        UIPasteboard.general.string = selectionText(rows: picked, start: selectionStart, end: selectionEnd)
        endSelection()
    }

    fileprivate func selectAll() {
        guard let grid = told else { return }
        selectionStart = At(col: 0, row: 0)
        selectionEnd = At(col: grid.cols - 1, row: grid.rows - 1)
        setNeedsDisplay()
        showMenu(at: CGPoint(x: bounds.midX, y: bounds.midY))
    }

    // ---- the keyboard ---------------------------------------------------

    /// A keyboard asked for before there was a size to be focused at.
    private var wanted = false

    func openKeyboard() {
        if bounds.width == 0 || bounds.height == 0 {
            wanted = true
            return
        }
        becomeFirstResponder()
    }

    func closeKeyboard() {
        wanted = false
        resignFirstResponder()
    }

    override var canBecomeFirstResponder: Bool { true }

    var hasText: Bool { true }
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardAppearance: UIKeyboardAppearance = .dark
    var keyboardType: UIKeyboardType = .asciiCapable
    var returnKeyType: UIReturnKeyType = .default

    func insertText(_ text: String) {
        if text == "\n" {
            send(held.enter)
        } else {
            send(held.typed(text))
        }
    }

    func deleteBackward() {
        send(held.backspace)
    }

    /// A hardware keyboard: the keys a soft one never sends, and chords.
    ///
    /// Anything else goes on to `insertText`, which already has the shifted
    /// character in it.
    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        var rest = Set<UIPress>()
        for press in presses {
            guard let key = press.key, handle(key) else {
                rest.insert(press)
                continue
            }
        }
        if !rest.isEmpty { super.pressesBegan(rest, with: event) }
    }

    private func handle(_ key: UIKey) -> Bool {
        // Held for this key only, and only if the row was not already holding
        // it: a key this does not handle must not leave shift lit behind it.
        let shifted = key.modifierFlags.contains(.shift) && !held.shift
        if shifted { held.shift = true }
        defer { if shifted { held.shift = false } }
        switch key.keyCode {
        case .keyboardUpArrow: cursorKey(.up)
        case .keyboardDownArrow: cursorKey(.down)
        case .keyboardRightArrow: cursorKey(.right)
        case .keyboardLeftArrow: cursorKey(.left)
        case .keyboardEscape: send(held.escape)
        case .keyboardTab: send(held.tab)
        case .keyboardReturnOrEnter where key.modifierFlags.contains(.shift): send(held.enter)
        default:
            guard key.modifierFlags.contains(.control),
                  let typed = key.charactersIgnoringModifiers.first
            else { return false }
            held.control = true
            send(held.character(typed))
        }
        return true
    }

    func cursorKey(_ direction: Direction) {
        leaveHistory()
        if let bytes = held.cursor(direction) {
            send(bytes)
        } else {
            attach?.cursorKey(direction: direction)
        }
    }

    func send(_ bytes: Data) {
        if bytes.isEmpty { return }
        leaveHistory()
        attach?.send(bytes: bytes)
    }

    func paste(_ text: String) {
        leaveHistory()
        attach?.paste(text: text)
    }

    /// Typing is asking for the session and not for the history: what is typed
    /// lands in a shell that has moved on since the lines being read.
    private func leaveHistory() {
        endSelection()
        if viewOpen {
            attach?.closeView()
            stopFling()
        }
    }
}

extension TerminalView: UIEditMenuInteractionDelegate {
    func editMenuInteraction(
        _ interaction: UIEditMenuInteraction,
        menuFor configuration: UIEditMenuConfiguration,
        suggestedActions: [UIMenuElement]
    ) -> UIMenu? {
        guard selection != nil else { return nil }
        return UIMenu(children: [
            UIAction(title: "Copy") { [weak self] _ in self?.copySelection() },
            UIAction(title: "Select All") { [weak self] _ in self?.selectAll() },
        ])
    }
}

/// The display link's target, holding the view weakly.
private final class Tick {
    weak var view: TerminalView?

    init(_ view: TerminalView) {
        self.view = view
    }

    @objc func fire(_ link: CADisplayLink) {
        view?.frame(link.timestamp)
    }
}
