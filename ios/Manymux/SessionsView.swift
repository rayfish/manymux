import SwiftUI

/// Everything running on one machine, as a wall of its screens or a list.
///
/// A name is what somebody called a session weeks ago and the screen is what
/// it is doing now, so the wall is the default. The screens come from
/// `Request::Peek`, which makes no client of the asker, so drawing the wall
/// does not tell the machine somebody is sitting at it.
struct SessionsView: View {
    @Environment(AppModel.self) private var model
    @Environment(\.scenePhase) private var phase
    let machine: Machine

    @SwiftUI.State private var wall: Wall?
    @SwiftUI.State private var trouble: Error?
    @SwiftUI.State private var starting = false
    @SwiftUI.State private var toast = Toast()

    private let columns = [GridItem(.adaptive(minimum: 150), spacing: 12)]

    var body: some View {
        ScrollView {
            content
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
        }
        .background(Color.ground)
        .navigationTitle(machine.address)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar { toolbar }
        .refreshable { await load() }
        // Asked again every two seconds while on the screen and in front, and
        // never while the app is in the background.
        .task(id: phase) {
            guard phase == .active else { return }
            while !Task.isCancelled {
                await load()
                try? await Task.sleep(for: .seconds(2))
            }
        }
        .toast(toast, at: .bottom, on: .panel)
    }

    @ViewBuilder
    private var content: some View {
        if let wall {
            sessions(wall)
        } else if let trouble {
            TroubleView(machine: machine, trouble: trouble) {
                self.trouble = nil
                Task { await load() }
            }
        } else {
            Text("reaching \(machine.address)")
                .font(.footnote)
                .foregroundStyle(Color.dim)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 12)
        }
    }

    @ViewBuilder
    private func sessions(_ wall: Wall) -> some View {
        if wall.running.isEmpty {
            Text("Nothing is running there yet. The + starts a session.")
                .font(.footnote)
                .foregroundStyle(Color.dim)
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.top, 12)
        } else if !wall.previews || !model.tiles {
            VStack(spacing: 6) {
                if !wall.previews {
                    Text("That machine is too old to show what is on the screens.")
                        .font(.footnote)
                        .foregroundStyle(Color.dim)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
                ForEach(wall.running, id: \.name) { session in
                    Button { model.attach(machine, session.name) } label: {
                        HStack {
                            Text(session.name)
                                .font(.system(size: 15, design: .monospaced))
                                .foregroundStyle(Color.text)
                                .lineLimit(1)
                            Spacer()
                            Idle(session: session)
                        }
                        .padding(14)
                        .background(Color.raised, in: RoundedRectangle(cornerRadius: 10))
                    }
                    .buttonStyle(.plain)
                }
            }
        } else {
            let screens = Dictionary(wall.screens.map { ($0.name, $0) }, uniquingKeysWith: { a, _ in a })
            LazyVGrid(columns: columns, spacing: 12) {
                ForEach(wall.running, id: \.name) { session in
                    Button { model.attach(machine, session.name) } label: {
                        Tile(session: session, screen: screens[session.name])
                    }
                    .buttonStyle(.plain)
                }
            }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .topBarTrailing) {
            // Only once there is an answer, and only for a machine that can be
            // peeked: a switch with one destination lies about what the app
            // can do. It shows what pressing it gives.
            if wall?.previews == true {
                Button {
                    model.tiles.toggle()
                } label: {
                    Image(systemName: model.tiles ? "list.bullet" : "square.grid.2x2")
                }
                .accessibilityLabel(model.tiles ? "show them as a list" : "show them as screens")
            }
            Menu {
                Button("New session here") { start() }
                    .disabled(starting)
                Button("Another machine") { model.home() }
            } label: {
                Image(systemName: "plus")
            }
            .accessibilityLabel("new session, or another machine")
        }
    }

    private func load() async {
        let answer = await model.ask { try $0.wall(machine: machine) }
        switch answer {
        case let .success(wall):
            self.wall = wall
            trouble = nil
            // Written only when it differs: every write is a redraw of every
            // view reading it, and most refreshes find nothing new.
            if model.seen[machine] != wall.running { model.seen[machine] = wall.running }
        case let .failure(error):
            // A list already on the screen stays there: one failed refresh is
            // a radio that dozed, and blanking the wall for it is a flicker.
            if wall == nil { trouble = error }
        }
    }

    /// A login shell there, opened as soon as it has a name.
    private func start() {
        starting = true
        toast.say("starting one")
        // A size to start it at; the real one is sent the moment the screen
        // knows its own, before the session has printed anything.
        model.ask({ try $0.startOn(machine: machine, grid: Grid(cols: 80, rows: 24)) }) { answer in
            starting = false
            switch answer {
            case let .success(name):
                model.attach(machine, name)
            case let .failure(error):
                toast.say((error as? Trouble)?.why ?? "could not start one")
            }
        }
    }
}

/// One session as a square: its screen, and underneath what it is called.
private struct Tile: View {
    let session: Running
    let screen: Preview?

    var body: some View {
        VStack(spacing: 0) {
            SnapshotView(preview: screen)
                .frame(height: 120)
            HStack {
                Text(session.name)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Color.text)
                    .lineLimit(1)
                Spacer(minLength: 4)
                Idle(session: session)
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 8)
        }
        .background(Color.raised)
        .clipShape(RoundedRectangle(cornerRadius: 10))
        .contentShape(RoundedRectangle(cornerRadius: 10))
    }
}

/// How long since anything was typed, and a dot for somebody already in it.
private struct Idle: View {
    let session: Running

    var body: some View {
        Text(session.attached > 0 ? "● \(ago(session.idle))" : ago(session.idle))
            .font(.caption2)
            .foregroundStyle(session.attached > 0 ? Color.accent : Color.hint)
    }
}

/// A machine that would not answer, and whatever can be done about it.
///
/// Which failure it was is read off the type rather than out of the words, so
/// a message reworded in the shim cannot take away the button that fixes it.
struct TroubleView: View {
    @Environment(AppModel.self) private var model
    let machine: Machine
    let trouble: Error
    let again: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text((trouble as? Trouble)?.why ?? trouble.localizedDescription)
                .font(.subheadline)
                .foregroundStyle(Color.warn)
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.raised, in: RoundedRectangle(cornerRadius: 6))
            switch trouble as? Trouble {
            case .Refused:
                // Where every machine starts: the account has never been given
                // this device's key, and the key is right here.
                Text("This device's key")
                    .font(.headline)
                    .foregroundStyle(Color.text)
                Text("Add this line to \(machine.user)'s authorized_keys on that machine.")
                    .font(.footnote)
                    .foregroundStyle(Color.dim)
                DeviceKey()
            case .HostKey:
                // A reinstalled machine or somebody in the middle, and only
                // the person reading this can say which.
                Button("it was reinstalled: forget the old key") {
                    model.ask({ try $0.forget(machine: machine) }) { _ in again() }
                }
                .tint(.warn)
            default:
                EmptyView()
            }
            Button("try again", action: again)
                .buttonStyle(.borderedProminent)
                .tint(.accent)
                .foregroundStyle(Color.ground)
            Button("another machine") { model.home() }
                .tint(.dim)
        }
        .padding(.top, 8)
    }
}
