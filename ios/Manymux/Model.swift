import Foundation
import Observation

/// The range the terminal's text size moves in.
enum TextSize {
    /// The Android app's 12.5dp, about fifty columns on an ordinary phone.
    static let normal = 12.5
    /// About as small as a phone's screen still reads, for the wide program
    /// that wants every column it can get.
    static let smallest = 7.0
    static let largest = 24.0

    /// In half-point steps, so a slider and a pinch land on the same sizes.
    static func clamped(_ size: Double) -> Double {
        (min(max(size, smallest), largest) * 2).rounded() / 2
    }
}

/// Where the app is.
enum Route: Hashable {
    case addHost
    case deviceKey
    case sessions(Machine)
    case session(Machine, String)
}

/// The app's state: this device, the saved hosts, and where you are.
///
/// One `Phone` for the life of the app. It owns the shim's runtime and its
/// pool of connections, which is what makes going back to the list and into
/// another session cost a round trip rather than a handshake.
@Observable
final class AppModel {
    /// Nil only if the device's key could not be read or made, which is what
    /// `trouble` says.
    let phone: Phone?
    let trouble: String?

    var hosts: [Machine]
    var path: [Route] = []

    /// What was running the last time a machine answered, per machine.
    ///
    /// Kept so the switcher over a session opens on something rather than on
    /// a round trip, the way the Android app keeps `seen`.
    var seen: [Machine: [Running]] = [:]

    /// Whether sessions are drawn as screens rather than as a list. About how
    /// somebody reads rather than about a machine, so it is remembered.
    var tiles: Bool {
        didSet { UserDefaults.standard.set(tiles, forKey: "tiles") }
    }

    /// The terminal's text size, in points. Like `tiles`, about the person
    /// reading rather than the machine, so one setting for every session.
    var textSize: Double {
        didSet { UserDefaults.standard.set(textSize, forKey: "textSize") }
    }

    private let store = HostStore()

    /// Anything that talks to a machine, off the main thread and one at a
    /// time. The shim's blocking calls say so, and are called from here only.
    private let elsewhere = DispatchQueue(label: "manymux.elsewhere", qos: .userInitiated)

    /// Whether a listing nobody is waiting for is already out.
    private var asking = false

    init() {
        hosts = store.all()
        tiles = UserDefaults.standard.object(forKey: "tiles") as? Bool ?? true
        let stored = UserDefaults.standard.double(forKey: "textSize")
        textSize = stored > 0 ? TextSize.clamped(stored) : TextSize.normal
        do {
            phone = try Phone.keptIn(dir: try AppModel.keptIn().path)
            trouble = nil
        } catch {
            phone = nil
            trouble = error.localizedDescription
        }
    }

    /// Application Support, kept out of backups: the key in there is this
    /// device's identity, and a restore onto another phone is a second device
    /// holding it. The Android app sets `allowBackup="false"` for the same.
    private static func keptIn() throws -> URL {
        var dir = try FileManager.default.url(
            for: .applicationSupportDirectory,
            in: .userDomainMask,
            appropriateFor: nil,
            create: true
        ).appendingPathComponent("manymux", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        var values = URLResourceValues()
        values.isExcludedFromBackup = true
        try dir.setResourceValues(values)
        return dir
    }

    // ---- hosts -----------------------------------------------------------

    func remember(_ machine: Machine) {
        hosts = [machine] + hosts.filter { $0 != machine }
        store.save(hosts)
    }

    func remove(_ machine: Machine) {
        hosts.removeAll { $0 == machine }
        store.save(hosts)
    }

    func open(_ machine: Machine) {
        remember(machine)
        path = [.sessions(machine)]
    }

    func home() {
        path = []
    }

    /// Into a session, or from one session straight to the next.
    func attach(_ machine: Machine, _ name: String) {
        if case .session = path.last {
            path[path.count - 1] = .session(machine, name)
        } else {
            path.append(.session(machine, name))
        }
    }

    // ---- asking machines -------------------------------------------------

    /// Run a blocking call on the shim, and answer on the main thread.
    func ask<T>(
        _ work: @escaping (Phone) throws -> T,
        then: @escaping (Result<T, Error>) -> Void
    ) {
        guard let phone else { return }
        elsewhere.async {
            let answer = Result { try work(phone) }
            DispatchQueue.main.async { then(answer) }
        }
    }

    /// The same, for a caller that can wait.
    func ask<T>(_ work: @escaping (Phone) throws -> T) async -> Result<T, Error> {
        await withCheckedContinuation { done in
            ask(work) { done.resume(returning: $0) }
        }
    }

    /// Ask what is running, for whoever looks next rather than now.
    ///
    /// At most one at a time: a listing over a link that has gone slow would
    /// otherwise leave the next one queued behind it for as long as ssh takes
    /// to give up. A failure says nothing, nobody having asked, and the copy
    /// already held stays, a stale list beating no list.
    func refresh(_ machine: Machine) {
        if asking { return }
        asking = true
        ask({ try $0.runningOn(machine: machine) }) { [weak self] answer in
            self?.asking = false
            if case let .success(running) = answer {
                self?.seen[machine] = running
            }
        }
    }
}

extension Machine {
    /// `user@address`, with the port only when it is not 22.
    var destination: String {
        "\(user)@\(address)" + (port == 22 ? "" : ":\(port)")
    }
}

extension Trouble {
    /// The sentence the shim wrote for the screen.
    var why: String {
        switch self {
        case let .Reaching(message), let .Refused(message), let .HostKey(message):
            return message
        }
    }
}

/// How long since anything was typed, in the one unit worth reading.
func ago(_ seconds: UInt64) -> String {
    switch seconds {
    case ..<60: return "now"
    case ..<3600: return "\(seconds / 60)m"
    case ..<86400: return "\(seconds / 3600)h"
    default: return "\(seconds / 86400)d"
    }
}
