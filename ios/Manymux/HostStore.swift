import Foundation

/// Saved destinations, with the most recently chosen first.
///
/// `UserDefaults` and JSON, the same shape the Android app keeps in its
/// preferences. An address and a user are not secrets; the key that gets in is
/// in the shim's own directory.
struct HostStore {
    private let defaults: UserDefaults
    private let key = "hosts"

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
    }

    private struct Saved: Codable {
        let address: String
        let port: Int
        let user: String
    }

    func all() -> [Machine] {
        guard let data = defaults.data(forKey: key),
              let saved = try? JSONDecoder().decode([Saved].self, from: data)
        else { return [] }
        var seen = Set<Machine>()
        return saved.compactMap { host in
            guard !host.address.isEmpty, !host.user.isEmpty, (1...65535).contains(host.port) else {
                return nil
            }
            let machine = Machine(address: host.address, port: UInt16(host.port), user: host.user)
            return seen.insert(machine).inserted ? machine : nil
        }
    }

    func remember(_ machine: Machine) {
        write([machine] + all().filter { $0 != machine })
    }

    func remove(_ machine: Machine) {
        write(all().filter { $0 != machine })
    }

    private func write(_ hosts: [Machine]) {
        let saved = hosts.map { Saved(address: $0.address, port: Int($0.port), user: $0.user) }
        if let data = try? JSONEncoder().encode(saved) {
            defaults.set(data, forKey: key)
        }
    }
}
