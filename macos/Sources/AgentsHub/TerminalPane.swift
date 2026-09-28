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
        // Spacing around the grid is ghostty's `window-padding-*`, from the user's own
        // config; this only fills what the surface leaves over.
        .background(Ghostty.background)
        .background(WindowTracker { model.trackWindow($0) })
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

/// The ⌘J panel: every mounted companion shell, stacked for the same reasons as
/// `TerminalPane`, with only the selected session's drawing.
struct ShellPane: View {
    @ObservedObject var model: AppModel

    var body: some View {
        let visible = model.visibleShellKey
        ZStack {
            if model.shellPanelOpen, !(visible.map { model.mountedShells.contains($0) } ?? false) {
                Text("starting shell…").foregroundStyle(.secondary)
            }
            ForEach(model.mountedShells, id: \.self) { key in
                if let terminal = model.terminals[key] {
                    TerminalSurfaceView(context: terminal.state)
                        .opacity(key == visible ? 1 : 0)
                        .allowsHitTesting(key == visible)
                        .accessibilityHidden(key != visible)
                }
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Ghostty.background)
    }
}

/// The panel's height as a share of the space the terminals get, so it keeps its
/// proportion through a window resize. Dragged from the handle, or set in Settings.
enum ShellPanelSize {
    static let key = "shellPanelFraction"
    static let initial = 0.35
    static let range = 0.15...0.85

    static func clamp(_ fraction: Double) -> Double {
        min(range.upperBound, max(range.lowerBound, fraction))
    }
}

/// Hands the model the window holding the panes, so it can ask whether that window is on
/// screen. Sits in the pane's own background rather than the root view so it can only ever
/// answer for the window the surfaces are in — the Settings window is one too.
///
/// The occlusion notification is a hint, not the source of truth: a view attaches before
/// its window is ordered in, and AppKit posts nothing for that first appearance, so the
/// state read here is always the one from before the window reached the screen. The model
/// re-reads `occlusionState` on its own tick; the notification only makes coming back from
/// hidden immediate rather than a tick late.
private struct WindowTracker: NSViewRepresentable {
    let onWindow: (NSWindow?) -> Void

    func makeNSView(context _: Context) -> Tracker {
        let tracker = Tracker()
        tracker.onWindow = onWindow
        return tracker
    }

    func updateNSView(_ tracker: Tracker, context _: Context) {
        tracker.onWindow = onWindow
    }

    final class Tracker: NSView {
        var onWindow: ((NSWindow?) -> Void)?

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            NotificationCenter.default.removeObserver(self)
            if let window {
                NotificationCenter.default.addObserver(
                    self,
                    selector: #selector(report),
                    name: NSWindow.didChangeOcclusionStateNotification,
                    object: window
                )
            }
            report()
        }

        @objc private func report() {
            onWindow?(window)
        }
    }
}
