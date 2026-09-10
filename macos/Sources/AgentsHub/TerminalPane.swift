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
        .frame(maxWidth: .infinity, maxHeight: .infinity)
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
