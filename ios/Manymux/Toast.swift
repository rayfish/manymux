import SwiftUI

/// A line said for two seconds over the screen and then gone, for news that
/// needs no answer.
@MainActor
@Observable
final class Toast {
    private(set) var said: String?

    func say(_ what: String) {
        withAnimation { said = what }
        Task {
            try? await Task.sleep(for: .seconds(2))
            withAnimation { if said == what { said = nil } }
        }
    }
}

extension View {
    func toast(_ toast: Toast, at edge: VerticalEdge, on fill: Color) -> some View {
        overlay(alignment: edge == .top ? .top : .bottom) {
            if let said = toast.said {
                Text(said)
                    .font(.footnote)
                    .foregroundStyle(Color.text)
                    .padding(.horizontal, 14)
                    .padding(.vertical, 10)
                    .background(fill, in: Capsule())
                    .padding(edge == .top ? .top : .bottom, edge == .top ? 12 : 24)
                    .transition(.opacity)
            }
        }
    }
}
