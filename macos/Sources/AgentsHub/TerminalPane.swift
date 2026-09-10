import AgentsHubCore
import GhosttyTerminal
import SwiftUI

/// Every mounted surface, stacked, with only the selected one drawing.
///
/// `opacity(0)` rather than `if`, `.hidden()` or a `TabView`, because a surface is only
/// built once its view is attached *and* has a non-zero size — a hidden or zero-size
/// container silently renders nothing, which is the same shape as the 0×0-pty trap the
/// Rust side already documents.
struct TerminalPane: View {
    @ObservedObject var model: AppModel

    /// Ghostty's own `window-padding-*` is left alone: it comes from the user's config
    /// and applies inside the surface, so adding to it there would double whatever they
    /// chose. This is the app's own breathing room around the grid.
    private let inset = EdgeInsets(top: 8, leading: 10, bottom: 8, trailing: 10)

    var body: some View {
        ZStack {
            if model.mounted.isEmpty {
                placeholder
            }
            ForEach(model.mounted, id: \.self) { key in
                if let terminal = model.terminals[key] {
                    TerminalSurfaceView(context: terminal.state)
                        .opacity(key == model.selectedKey ? 1 : 0)
                        .allowsHitTesting(key == model.selectedKey)
                        .accessibilityHidden(key != model.selectedKey)
                }
            }
        }
        .padding(inset)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        // The terminal's own colour, so the inset reads as part of the terminal rather
        // than a frame drawn around it.
        .background(Ghostty.background)
    }

    private var placeholder: some View {
        VStack(spacing: 6) {
            Text("Select a session, or press ⌘N to start one.")
                .foregroundStyle(.secondary)
            if let e = model.configError {
                Text(e).font(.caption).foregroundStyle(.red)
            }
        }
    }

}
