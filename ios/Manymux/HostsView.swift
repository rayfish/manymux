import SwiftUI

/// Saved hosts are the home screen; adding one and the device's key have pages
/// of their own.
struct HostsView: View {
    @Environment(AppModel.self) private var model

    private let columns = [GridItem(.adaptive(minimum: 300), spacing: 12)]

    var body: some View {
        ScrollView {
            if let trouble = model.trouble {
                Text(trouble)
                    .foregroundStyle(Color.warn)
                    .padding()
            }
            if model.hosts.isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    Text("No hosts yet")
                        .font(.title2)
                        .foregroundStyle(Color.text)
                    Text("Save an SSH host to open its running sessions.")
                        .font(.footnote)
                        .foregroundStyle(Color.dim)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(.horizontal, 24)
                .padding(.top, 36)
            }
            LazyVGrid(columns: columns, spacing: 12) {
                ForEach(model.hosts, id: \.self) { host in
                    HostCard(host: host)
                }
            }
            .padding(.horizontal, 20)
            .padding(.vertical, 8)
        }
        .background(Color.ground)
        .navigationTitle("Hosts")
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                NavigationLink("SSH key", value: Route.deviceKey)
                    .foregroundStyle(Color.dim)
            }
            ToolbarItem(placement: .topBarTrailing) {
                NavigationLink(value: Route.addHost) {
                    Label("Add host", systemImage: "plus")
                }
                .tint(.accent)
            }
        }
    }
}

private struct HostCard: View {
    @Environment(AppModel.self) private var model
    let host: Machine

    var body: some View {
        HStack(spacing: 14) {
            Button {
                model.open(host)
            } label: {
                HStack(spacing: 14) {
                    Text(">_")
                        .font(.system(size: 17, design: .monospaced))
                        .foregroundStyle(Color.accent)
                        .frame(width: 44, height: 44)
                        .background(Color.accentSurface, in: RoundedRectangle(cornerRadius: 12))
                        .accessibilityHidden(true)
                    VStack(alignment: .leading, spacing: 6) {
                        Text(host.address)
                            .font(.body.weight(.medium))
                            .foregroundStyle(Color.text)
                            .lineLimit(1)
                        Text("\(host.user)  ·  port \(host.port)")
                            .font(.caption)
                            .foregroundStyle(Color.dim)
                            .lineLimit(1)
                    }
                    Spacer(minLength: 0)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            .accessibilityLabel("connect \(host.destination)")

            Menu {
                Button("Remove host", role: .destructive) { model.remove(host) }
            } label: {
                Image(systemName: "ellipsis")
                    .foregroundStyle(Color.dim)
                    .frame(width: 40, height: 48)
                    .contentShape(Rectangle())
            }
            .accessibilityLabel("options for \(host.destination)")
        }
        .padding(.leading, 16)
        .padding(.vertical, 14)
        .background(Color.raised, in: RoundedRectangle(cornerRadius: 16))
    }
}

struct AddHostView: View {
    @Environment(AppModel.self) private var model
    @SwiftUI.State private var address = ""
    @SwiftUI.State private var user = ""
    @SwiftUI.State private var port = "22"
    @SwiftUI.State private var problem: String?

    var body: some View {
        Form {
            Section {
                TextField("gpu-box.example", text: $address)
                    .textContentType(.URL)
                    .keyboardType(.URL)
                    .accessibilityIdentifier("address")
            } header: {
                Text("Address")
            }
            Section("Username") {
                TextField("user", text: $user)
                    .textContentType(.username)
                    .accessibilityIdentifier("user")
            }
            Section("Port") {
                TextField("22", text: $port)
                    .keyboardType(.numberPad)
                    .accessibilityIdentifier("port")
            }
            if let problem {
                Text(problem).foregroundStyle(Color.warn)
            }
            Section {
                Button("Save host") {
                    if let machine = destination() {
                        model.remember(machine)
                        model.home()
                    }
                }
                Button("Connect now") {
                    if let machine = destination() { model.open(machine) }
                }
            }
        }
        .textInputAutocapitalization(.never)
        .autocorrectionDisabled()
        .scrollContentBackground(.hidden)
        .background(Color.ground)
        .navigationTitle("Add host")
        .onAppear {
            if user.isEmpty { user = model.hosts.first?.user ?? "" }
        }
    }

    private func destination() -> Machine? {
        let host = address.trimmingCharacters(in: .whitespaces)
        let account = user.trimmingCharacters(in: .whitespaces)
        if host.isEmpty || account.isEmpty {
            problem = "it wants an address and a user"
            return nil
        }
        guard let number = UInt16(port.trimmingCharacters(in: .whitespaces)), number > 0 else {
            problem = "port must be between 1 and 65535"
            return nil
        }
        return Machine(address: host, port: number, user: account)
    }
}

struct DeviceKeyView: View {
    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                Text("Add this public key to the host's authorized_keys to allow access.")
                    .font(.footnote)
                    .foregroundStyle(Color.dim)
                DeviceKey()
            }
            .padding(20)
        }
        .background(Color.ground)
        .navigationTitle("SSH key")
    }
}

/// The line to paste into a machine's `authorized_keys`, and a way to copy it.
///
/// That line is in this app and in no other place a phone can reach, so it is
/// shown wherever a machine has asked for it rather than only on its own page.
struct DeviceKey: View {
    @Environment(AppModel.self) private var model
    @SwiftUI.State private var copied = false

    var body: some View {
        let line = model.phone?.authorizedLine() ?? ""
        VStack(alignment: .leading, spacing: 12) {
            Text(line)
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(Color.dim)
                .textSelection(.enabled)
                .padding(14)
                .frame(maxWidth: .infinity, alignment: .leading)
                .background(Color.raised, in: RoundedRectangle(cornerRadius: 6))
            Button(copied ? "copied" : "copy the key") {
                UIPasteboard.general.string = line
                copied = true
            }
            .tint(.accent)
        }
    }
}
