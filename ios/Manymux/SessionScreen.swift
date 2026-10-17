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
    let machine: Machine
    let name: String

    @SwiftUI.State private var attachment = Attachment()
    @SwiftUI.State private var held = Held()
    @SwiftUI.State private var terminal: TerminalView?
    @SwiftUI.State private var toast = Toast()

    var body: some View {
        VStack(spacing: 0) {
            TerminalSurface(held: held, made: { terminal = $0 }) { view, grid in
                guard attachment.attach == nil, let phone = model.phone else { return }
                view.onCannotScroll = { toast.say("\(name) is on a machine too old to scroll back") }
                view.attach = attachment.open(phone, machine, name, grid)
            }
            ExtraKeys(held: held, terminal: terminal)
        }
        .background(Color.panel)
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
            ToolbarItem(placement: .topBarTrailing) { switcher }
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
        .onDisappear {
            terminal?.attach = nil
            attachment.leave()
        }
    }

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
    private var switcher: some View {
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
            Image(systemName: "rectangle.stack")
        }
        .accessibilityLabel("the other sessions on this machine")
    }
}

/// The terminal view, inside SwiftUI.
private struct TerminalSurface: UIViewRepresentable {
    let held: Held
    let made: (TerminalView) -> Void
    let grid: (TerminalView, Grid) -> Void

    func makeUIView(context: Context) -> TerminalView {
        let view = TerminalView(held: held)
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

    func updateUIView(_ view: TerminalView, context: Context) {}

    static func dismantleUIView(_ view: TerminalView, coordinator: ()) {
        view.onGrid = nil
        view.attach = nil
    }
}

/// The keys a phone keyboard has not got.
private struct ExtraKeys: View {
    let held: Held
    let terminal: TerminalView?

    var body: some View {
        VStack(spacing: 2) {
            HStack(spacing: 2) {
                key("esc") { terminal?.send(held.escape) }
                key("tab") { terminal?.send(held.tab) }
                key("ctrl", lit: held.control) { held.control.toggle() }
                key("shift", lit: held.shift) { held.shift.toggle() }
                key("up", symbol: "arrow.up") { terminal?.cursorKey(.up) }
                Color.clear.frame(maxWidth: .infinity, maxHeight: 32)
            }
            HStack(spacing: 2) {
                // The system's own paste button, which reads the clipboard
                // without the prompt a programmatic read sets off.
                PasteButton(payloadType: String.self) { strings in
                    guard let text = strings.first, !text.isEmpty else { return }
                    DispatchQueue.main.async { terminal?.paste(text) }
                }
                .labelStyle(.iconOnly)
                .buttonBorderShape(.roundedRectangle(radius: 8))
                .tint(.raised)
                .frame(maxWidth: .infinity, maxHeight: 32)
                key("keyboard", symbol: "keyboard") {
                    if terminal?.isFirstResponder == true {
                        terminal?.closeKeyboard()
                    } else {
                        terminal?.openKeyboard()
                    }
                }
                Color.clear.frame(maxWidth: .infinity, maxHeight: 32)
                key("left", symbol: "arrow.left") { terminal?.cursorKey(.left) }
                key("down", symbol: "arrow.down") { terminal?.cursorKey(.down) }
                key("right", symbol: "arrow.right") { terminal?.cursorKey(.right) }
            }
        }
        .padding(2)
        .background(Color.panel)
    }

    /// A key that says `label`, or shows `symbol` and is called `label` out
    /// loud.
    private func key(
        _ label: String,
        symbol: String? = nil,
        lit: Bool = false,
        press: @escaping () -> Void
    ) -> some View {
        Button(action: press) {
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
        .buttonStyle(.plain)
        .accessibilityLabel(label)
        .accessibilityAddTraits(lit ? .isSelected : [])
    }
}
