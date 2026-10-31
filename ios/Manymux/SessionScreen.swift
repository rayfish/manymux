import SwiftUI
import UIKit

/// One attach, owned by the screen showing it.
///
/// Made when the terminal first knows its grid, since attaching needs a size
/// to ask for, and let go of when the screen goes. Leaving detaches and ends
/// nothing on the machine.
@Observable
final class Attachment {
    private(set) var state: State = .reaching
    private(set) var attach: Attach?
    private var ticking: Timer?

    func open(_ phone: Phone, _ machine: Machine, _ name: String, _ grid: Grid) -> Attach {
        // Returns at once: the connection, the ladder and the attach happen on
        // the shim's own runtime, and `state` says how they are going.
        let attach = phone.attach(machine: machine, name: name, grid: grid)
        self.attach = attach
        state = attach.state()
        ticking = Timer.scheduledTimer(withTimeInterval: 0.5, repeats: true) { [weak self] _ in
            guard let self, let attach = self.attach else { return }
            let now = attach.state()
            if now != self.state { self.state = now }
        }
        return attach
    }

    func leave() {
        ticking?.invalidate()
        ticking = nil
        attach?.detach()
        attach = nil
    }
}

struct SessionScreen: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @Environment(\.scenePhase) private var phase
    @Environment(\.verticalSizeClass) private var verticalSize
    let machine: Machine
    let name: String

    @SwiftUI.State private var attachment = Attachment()
    @SwiftUI.State private var held = Held()
    @SwiftUI.State private var terminal: TerminalView?
    @SwiftUI.State private var toast = Toast()
    @SwiftUI.State private var wasBackgrounded = false

    var body: some View {
        VStack(spacing: 0) {
            TerminalSurface(
                held: held,
                textSize: model.textSize,
                resized: { model.textSize = Double($0) },
                made: { terminal = $0 }
            ) { view, grid in
                guard attachment.attach == nil, let phone = model.phone else { return }
                view.onCannotScroll = { toast.say("\(name) is on a machine too old to scroll back") }
                view.attach = attachment.open(phone, machine, name, grid)
            }
            .overlay(alignment: .top) {
                // With no bar to say it in, only news is shown: a session
                // that is attached is the one whose screen is under it.
                if flat && attachment.state != .attached {
                    Text(status)
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundStyle(statusColour)
                        .lineLimit(1)
                        .truncationMode(.middle)
                        .padding(.horizontal, 12)
                        .padding(.vertical, 6)
                        .background(Color.raised, in: Capsule())
                        .padding(.top, 6)
                }
            }
            ExtraKeys(held: held, terminal: terminal, flat: flat, leave: { dismiss() }) {
                switcher { keyFace("the other sessions", symbol: "rectangle.stack") }
            }
        }
        .background(Color.panel)
        .toolbar(flat ? .hidden : .visible, for: .navigationBar)
        .navigationBarTitleDisplayMode(.inline)
        .toolbarBackground(Color.panel, for: .navigationBar)
        .toolbarBackground(.visible, for: .navigationBar)
        .toolbar {
            ToolbarItem(placement: .principal) {
                Text(status)
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundStyle(statusColour)
                    .lineLimit(1)
                    .truncationMode(.middle)
            }
            ToolbarItem(placement: .topBarTrailing) {
                switcher { Image(systemName: "rectangle.stack") }
            }
        }
        .toast(toast, at: .top, on: .raised)
        // What else is running, asked while nobody is waiting on the answer,
        // so the switcher opens on the machine as it is rather than as it was
        // when the list was last drawn.
        .task {
            while !Task.isCancelled {
                model.refresh(machine)
                try? await Task.sleep(for: .seconds(15))
            }
        }
        .onChange(of: phase) { _, now in
            if now == .background {
                wasBackgrounded = true
            } else if now == .active && wasBackgrounded {
                wasBackgrounded = false
                attachment.attach?.resume()
            }
        }
        .onDisappear {
            terminal?.attach = nil
            attachment.leave()
        }
    }

    /// A phone on its side, where the bar and two rows of keys would leave the
    /// terminal a strip: the bar goes and its two buttons join one row of keys.
    private var flat: Bool { verticalSize == .compact }

    /// The bar says where you are, and how it is going. The screen under it
    /// stays as the session last painted it whatever happens to the
    /// connection; only this line changes.
    private var status: String {
        let place = "\(machine.address)/\(name)"
        switch attachment.state {
        case .reaching: return "\(place)  ·  reaching"
        case .attached: return place
        case let .waiting(tries): return "\(place)  ·  reconnecting (\(tries))"
        case let .ended(status): return "\(place)  ·  ended \(status)"
        case .detached: return "\(place)  ·  detached"
        case let .failed(why): return why
        }
    }

    private var statusColour: Color {
        switch attachment.state {
        case .attached: return .text
        case .failed, .waiting: return .warn
        default: return .dim
        }
    }

    /// The way to the session next door, where a desktop has `Ctrl-] tab`.
    ///
    /// Opened on the listing already in hand rather than on a round trip, so
    /// it costs nothing to press. The session you are in is shown and not
    /// offered, so the rows do not move under a hand that has learnt them.
    private func switcher(@ViewBuilder label: () -> some View) -> some View {
        Menu {
            let seen = model.seen[machine] ?? []
            if seen.isEmpty {
                Text("nothing else answered")
            }
            ForEach(seen, id: \.name) { session in
                if session.name == name {
                    Label(session.name, systemImage: "circle.fill")
                } else {
                    Button(session.name) { model.attach(machine, session.name) }
                }
            }
        } label: {
            label()
        }
        .accessibilityLabel("the other sessions on this machine")
    }
}

/// The terminal view, inside SwiftUI.
private struct TerminalSurface: UIViewRepresentable {
    let held: Held
    let textSize: Double
    let resized: (CGFloat) -> Void
    let made: (TerminalView) -> Void
    let grid: (TerminalView, Grid) -> Void

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(held: held)
        view.textSize = CGFloat(textSize)
        view.onTextSize = resized
        view.onGrid = { [weak view] grid in
            guard let view else { return }
            self.grid(view, grid)
        }
        DispatchQueue.main.async {
            made(view)
            view.openKeyboard()
        }
        return view
    }

    func updateUIView(_ view: TerminalView, context: Context) {
        view.textSize = CGFloat(textSize)
    }

    static func dismantleUIView(_ view: TerminalView, coordinator: ()) {
        view.onGrid = nil
        view.onTextSize = nil
        view.attach = nil
    }
}

/// The keys a phone keyboard has not got.
///
/// Two rows upright, one on its side, where the row also carries the way back
/// and `ends`, the switcher, since the bar that held them is gone.
private struct ExtraKeys<Ends: View>: View {
    let held: Held
    let terminal: TerminalView?
    let flat: Bool
    let leave: () -> Void
    @ViewBuilder let ends: () -> Ends

    var body: some View {
        Group {
            if flat {
                HStack(spacing: 6) {
                    key("back", symbol: "chevron.left", press: leave)
                    esc
                    tab
                    control
                    shift
                    paste
                    keyboard
                    key("left", symbol: "arrow.left") { terminal?.cursorKey(.left) }
                    key("up", symbol: "arrow.up") { terminal?.cursorKey(.up) }
                    key("down", symbol: "arrow.down") { terminal?.cursorKey(.down) }
                    key("right", symbol: "arrow.right") { terminal?.cursorKey(.right) }
                    ends()
                }
            } else {
                VStack(spacing: 6) {
                    HStack(spacing: 6) {
                        esc
                        tab
                        control
                        shift
                        key("up", symbol: "arrow.up") { terminal?.cursorKey(.up) }
                        Color.clear.frame(maxWidth: .infinity, maxHeight: 32)
                    }
                    HStack(spacing: 6) {
                        paste
                        keyboard
                        Color.clear.frame(maxWidth: .infinity, maxHeight: 32)
                        key("left", symbol: "arrow.left") { terminal?.cursorKey(.left) }
                        key("down", symbol: "arrow.down") { terminal?.cursorKey(.down) }
                        key("right", symbol: "arrow.right") { terminal?.cursorKey(.right) }
                    }
                }
            }
        }
        .padding(.horizontal, 8)
        .padding(.vertical, flat ? 4 : 8)
        .background(Color.panel)
    }

    private var esc: some View { key("esc") { terminal?.send(held.escape) } }
    private var tab: some View { key("tab") { terminal?.send(held.tab) } }
    private var control: some View {
        key("ctrl", lit: held.control) { held.control.toggle() }
    }
    private var shift: some View {
        key("shift", lit: held.shift) { held.shift.toggle() }
    }

    /// The system's own paste button, which reads the clipboard without the
    /// prompt a programmatic read sets off.
    private var paste: some View {
        PasteButton(payloadType: String.self) { strings in
            guard let text = strings.first, !text.isEmpty else { return }
            DispatchQueue.main.async { terminal?.paste(text) }
        }
        .labelStyle(.iconOnly)
        .buttonBorderShape(.roundedRectangle(radius: 8))
        .tint(.raised)
        .frame(maxWidth: .infinity, maxHeight: 32)
    }

    private var keyboard: some View {
        key("keyboard", symbol: "keyboard") {
            if terminal?.isFirstResponder == true {
                terminal?.closeKeyboard()
            } else {
                terminal?.openKeyboard()
            }
        }
    }

    private func key(
        _ label: String,
        symbol: String? = nil,
        lit: Bool = false,
        press: @escaping () -> Void
    ) -> some View {
        Button(action: press) {
            keyFace(label, symbol: symbol, lit: lit)
        }
        .buttonStyle(.plain)
        .accessibilityLabel(label)
        .accessibilityAddTraits(lit ? .isSelected : [])
    }
}

/// What a key looks like: `label`, or `symbol` and called `label` out loud.
/// Apart from the button so the switcher's menu can wear it too.
private func keyFace(_ label: String, symbol: String? = nil, lit: Bool = false) -> some View {
    Group {
        if let symbol {
            Image(systemName: symbol).font(.system(size: 14, weight: .medium))
        } else {
            Text(label).font(.system(size: 12))
        }
    }
    .foregroundStyle(lit ? Color.accent : Color.text)
    .frame(maxWidth: .infinity, minHeight: 32, maxHeight: 32)
    .background(
        RoundedRectangle(cornerRadius: 8)
            .fill(lit ? Color.accentSurface : Color.raised)
            .stroke(Color.line, lineWidth: 1)
    )
    .contentShape(Rectangle())
}
