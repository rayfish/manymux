import SwiftUI
import UIKit

@main
struct ManymuxApp: App {
    @SwiftUI.State private var model = AppModel()
    @Environment(\.scenePhase) private var phase
    @SwiftUI.State private var lingering: UIBackgroundTaskIdentifier = .invalid

    var body: some Scene {
        WindowGroup {
            @Bindable var model = model
            NavigationStack(path: $model.path) {
                HostsView()
                    .navigationDestination(for: Route.self) { route in
                        switch route {
                        case .addHost:
                            AddHostView()
                        case .deviceKey:
                            DeviceKeyView()
                        case .settings:
                            SettingsView()
                        case let .sessions(machine):
                            SessionsView(machine: machine)
                        case let .session(machine, name):
                            SessionScreen(machine: machine, name: name)
                                // A new identity per session, so a switch
                                // tears down one attach and builds the next.
                                .id(route)
                        }
                    }
            }
            .environment(model)
            .tint(.accent)
            .preferredColorScheme(.dark)
        }
        .onChange(of: phase) { _, now in linger(now) }
    }

    /// Ask for the half minute iOS gives an app on its way to the background.
    ///
    /// There is no foreground service here: a suspended app's sockets go
    /// quiet, and the host detaches a client that has stopped answering pings.
    /// What that costs is nothing on the machine, the session being the node's,
    /// and a reconnect when the app comes back, which the attach does on its
    /// own. The half minute is so a glance at another app is not even that.
    private func linger(_ now: ScenePhase) {
        switch now {
        case .background:
            guard lingering == .invalid else { return }
            lingering = UIApplication.shared.beginBackgroundTask(withName: "attached") {
                UIApplication.shared.endBackgroundTask(lingering)
                lingering = .invalid
            }
        case .active:
            guard lingering != .invalid else { return }
            UIApplication.shared.endBackgroundTask(lingering)
            lingering = .invalid
        default:
            break
        }
    }
}
