import SwiftUI
import UIKit

/// What the app remembers about the person using it, rather than about a
/// machine: how big the terminal's text is, and how sessions are listed.
struct SettingsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        @Bindable var model = model
        ScrollView {
            VStack(alignment: .leading, spacing: 28) {
                section("Text size") {
                    TextSizePreview(size: model.textSize)
                    TextSizeSlider(size: $model.textSize)
                    Text("Pinching a session's screen changes this too.")
                        .font(.footnote)
                        .foregroundStyle(Color.hint)
                }
                section("Sessions") {
                    Toggle(isOn: $model.tiles) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text("Show sessions as screens")
                                .foregroundStyle(Color.text)
                            Text("A picture of each session rather than a list of names.")
                                .font(.footnote)
                                .foregroundStyle(Color.dim)
                        }
                    }
                    .tint(.accent)
                    .padding(14)
                    .background(Color.raised, in: RoundedRectangle(cornerRadius: 10))
                }
                section("This device") {
                    NavigationLink(value: Route.deviceKey) {
                        HStack {
                            Image(systemName: "key")
                                .foregroundStyle(Color.accent)
                            Text("SSH key")
                                .foregroundStyle(Color.text)
                            Spacer()
                            Image(systemName: "chevron.right")
                                .font(.footnote.weight(.semibold))
                                .foregroundStyle(Color.hint)
                        }
                        .padding(14)
                        .background(Color.raised, in: RoundedRectangle(cornerRadius: 10))
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(20)
        }
        .background(Color.ground)
        .navigationTitle("Settings")
    }

    private func section(
        _ title: String,
        @ViewBuilder content: () -> some View
    ) -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(title.uppercased())
                .font(.caption.weight(.semibold))
                .foregroundStyle(Color.dim)
            content()
        }
    }
}

/// A few lines of a terminal at the size being chosen, and how many columns
/// that leaves across this screen, which is the number the choice is about.
private struct TextSizePreview: View {
    let size: Double

    var body: some View {
        let painter = Painter(size: CGFloat(size))
        let font = Font(painter.regular)
        VStack(alignment: .leading, spacing: 0) {
            Group {
                (Text("~/project ").foregroundStyle(Color.accent)
                    + Text("$ ").foregroundStyle(Color.dim)
                    + Text("cargo test"))
                Text("   Compiling manymux v0.2.1").foregroundStyle(Color.dim)
                Text("    Finished `test` profile in 4.21s").foregroundStyle(Color.dim)
                Text("running 128 tests").foregroundStyle(Color.dim)
                (Text("test result: ") + Text("ok").foregroundStyle(Color.accent)
                    + Text(". 128 passed; 0 failed"))
                (Text("~/project ").foregroundStyle(Color.accent)
                    + Text("$ ").foregroundStyle(Color.dim)
                    + Text("▊").foregroundStyle(Color.text))
            }
            .font(font)
            .foregroundStyle(Color.text)
            .lineLimit(1)
            .frame(height: painter.line, alignment: .leading)
        }
        .frame(maxWidth: .infinity, minHeight: 120, alignment: .topLeading)
        .padding(14)
        .clipped()
        .background(Color.ground, in: RoundedRectangle(cornerRadius: 10))
        .overlay(RoundedRectangle(cornerRadius: 10).stroke(Color.line, lineWidth: 1))
        .overlay(alignment: .bottomTrailing) {
            Text("\(columns(painter)) columns")
                .font(.system(size: 11, design: .monospaced))
                .foregroundStyle(Color.hint)
                .padding(10)
        }
        .animation(.easeOut(duration: 0.12), value: size)
    }

    /// Across the screen as it is held now, the terminal being as wide as it.
    private func columns(_ painter: Painter) -> Int {
        let scene = UIApplication.shared.connectedScenes.first as? UIWindowScene
        let width = scene?.screen.bounds.width ?? 390
        return max(Int(width / painter.cell), 1)
    }
}

/// The slider, with the two ends drawn as the smallest and largest text, the
/// size it is at, and a way back to where it started.
private struct TextSizeSlider: View {
    @Binding var size: Double

    var body: some View {
        VStack(spacing: 10) {
            HStack(spacing: 14) {
                Image(systemName: "textformat.size.smaller")
                    .font(.system(size: 13))
                    .foregroundStyle(Color.dim)
                Slider(value: $size, in: TextSize.smallest...TextSize.largest, step: 0.5)
                    .tint(.accent)
                    .accessibilityValue("\(label) points")
                Image(systemName: "textformat.size.larger")
                    .font(.system(size: 19))
                    .foregroundStyle(Color.dim)
            }
            HStack {
                Text("\(label) pt")
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundStyle(Color.text)
                    .contentTransition(.numericText(value: size))
                Spacer()
                Button("Default") {
                    withAnimation { size = TextSize.normal }
                }
                .font(.footnote)
                .tint(.accent)
                .disabled(size == TextSize.normal)
            }
        }
        .padding(14)
        .background(Color.raised, in: RoundedRectangle(cornerRadius: 10))
        .sensoryFeedback(.selection, trigger: size)
    }

    private var label: String {
        size.rounded() == size ? String(Int(size)) : String(format: "%.1f", size)
    }
}
