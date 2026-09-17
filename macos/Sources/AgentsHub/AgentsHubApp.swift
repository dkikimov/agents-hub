import AgentsHubCore
import SwiftUI

/// Folders arrive here from a Dock drop, Finder's Open With, and `agents-hub open`.
/// `.onOpenURL` is not an option: it fires for URL schemes, not for a `CFBundleDocumentTypes`
/// file open, which only AppKit's delegate sees.
///
/// A cold launch delivers the drop before the scene's `.task` has run `AppModel.start`, so an
/// open with nobody yet to take it waits in `pending` until there is.
final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor static var onOpen: (([String]) -> Void)?
    @MainActor static var pending: [String] = []

    func application(_ sender: NSApplication, open urls: [URL]) {
        let paths = urls.filter(\.hasDirectoryPath).map(\.path)
        Task { @MainActor in
            if let onOpen = Self.onOpen { onOpen(paths) } else { Self.pending += paths }
        }
    }
}

@main
struct AgentsHubApp: App {
    @StateObject private var model = AppModel()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @AppStorage("appearance") private var appearance = AppAppearance.dark
    @AppStorage("theme") private var theme = Theme.classic

    var body: some Scene {
        WindowGroup {
            RootView(model: model, appearance: appearance)
                .task {
                    // Launched from a shell rather than Finder, the app otherwise opens
                    // behind whatever is frontmost. Never `ignoringOtherApps` — that
                    // steals keystrokes mid-sentence into whichever session is attached.
                    NSApp.activate()
                    model.start()
                }
                .onDisappear { model.stop() }
        }
        .defaultSize(width: 1280, height: 820)
        // One short band instead of titlebar-plus-toolbar; the header is a line of text.
        .windowToolbarStyle(.unifiedCompact)
        .commands {
            CommandGroup(after: .newItem) {
                Button("New Session…") { model.sheet = .newSession }
                    .keyboardShortcut("n", modifiers: .command)
                Divider()
                Button("Restart Session") { model.restartSelected() }
                    .keyboardShortcut("r", modifiers: [.command, .shift])
                Button("Kill Session…") { model.killSelected() }
                    .keyboardShortcut(.delete, modifiers: .command)
            }
            CommandMenu("View") {
                // ⌘L is the way back out of a focused terminal: once ghostty has the
                // keyboard it takes everything except the command layer.
                Button("Focus Sidebar") { model.requestSidebarFocus.toggle() }
                    .keyboardShortcut("l", modifiers: .command)
                Divider()
                Picker("Appearance", selection: $appearance) {
                    ForEach(AppAppearance.allCases) { Text($0.label).tag($0) }
                }
                Picker("Theme", selection: $theme) {
                    ForEach(Theme.allCases) { Text($0.label).tag($0) }
                }
            }
        }

        // ⌘, for free, and the window macOS users look for when a menu picker isn't enough.
        Settings { SettingsView() }
    }
}

struct SettingsView: View {
    @AppStorage("appearance") private var appearance = AppAppearance.dark
    @AppStorage("theme") private var theme = Theme.classic

    var body: some View {
        Form {
            Picker("Appearance", selection: $appearance) {
                ForEach(AppAppearance.allCases) { Text($0.label).tag($0) }
            }
            Picker("Theme", selection: $theme) {
                ForEach(Theme.allCases) { Text($0.label).tag($0) }
            }
            LabeledContent("Palette") {
                HStack(spacing: 4) {
                    ForEach(Array(swatches.enumerated()), id: \.offset) { _, color in
                        RoundedRectangle(cornerRadius: 3).fill(color).frame(width: 18, height: 14)
                    }
                }
            }
            Text("Terminal colours and font come from your own ghostty config; a theme here "
                 + "only paints the app around it.")
                .font(Ghostty.ui(Ghostty.fontSize - 2))
                .foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
    }

    private var swatches: [Color] {
        let p = theme.palette
        return [p.vm, p.folder, p.agent, p.running, p.attention, p.accent]
    }
}

struct RootView: View {
    @ObservedObject var model: AppModel
    let appearance: AppAppearance

    @Environment(\.colorScheme) private var systemScheme
    @FocusState private var focus: FocusTarget?
    @AppStorage("theme") private var theme = Theme.classic
    @AppStorage("sidebarWidth") private var sidebarWidth = 270.0
    @State private var widthAtDragStart: Double?

    private var scheme: ColorScheme { appearance.colorScheme ?? systemScheme }

    var body: some View {
        // Neither split view fits: NavigationSplitView draws its sidebar column as an inset
        // rounded card with a hairline outline that no list style or background gets under,
        // and HSplitView's divider is a hardcoded black NSSplitView draws itself. Two panes
        // and a `Divider` is the whole requirement, so it is the whole implementation.
        HStack(spacing: 0) {
            SidebarView(model: model, focus: $focus)
                .frame(width: sidebarWidth)
            splitHandle
            VStack(spacing: 0) {
                TerminalPane(model: model)
                    .focused($focus, equals: .terminal)
                Divider()
                statusBar
            }
            .frame(maxWidth: .infinity)
        }
        // No `navigationTitle`: the window would draw it in the system font, which is the
        // one seam you notice above a monospaced UI. `header` carries the same information
        // in the terminal's own font, in the titlebar the window already reserves.
        .withoutToolbarTitle()
        .toolbar { headerItem }
        .preferredColorScheme(appearance.colorScheme)
        // The terminal has its own notion of light/dark: a ghostty config using
        // `theme = "light:…,dark:…"` needs telling, and so does "Apple System Colors".
        .onAppear {
            Ghostty.apply(scheme)
            // Start in the list, so j/k work without clicking first.
            focus = .sidebar
        }
        .onChange(of: scheme) { _, new in Ghostty.apply(new) }
        .onChange(of: model.requestSidebarFocus) { _, _ in focus = .sidebar }
        .sheet(item: $model.sheet) { sheet in
            switch sheet {
            case .newSession:
                NewSessionSheet(model: model)
            case let .confirmKill(key, label):
                KillConfirm(model: model, key: key, label: label)
            }
        }
    }

    /// The `Divider` is the visible pane edge; the clear strip over it is the grab area,
    /// wider than a hairline because a 1pt drag target is a 1pt drag target.
    private var splitHandle: some View {
        Divider()
            .overlay {
                Color.clear
                    .frame(width: 9)
                    .contentShape(.rect)
                    .onHover { $0 ? NSCursor.resizeLeftRight.push() : NSCursor.pop() }
                    .gesture(
                        DragGesture(minimumDistance: 1)
                            .onChanged { drag in
                                let start = widthAtDragStart ?? sidebarWidth
                                widthAtDragStart = start
                                sidebarWidth = min(440, max(190, start + drag.translation.width))
                            }
                            .onEnded { _ in widthAtDragStart = nil }
                    )
            }
    }

    private var subtitle: String {
        guard let key = model.selectedKey, let info = model.selectedInfo else { return "" }
        let vm = model.vms[safe: key.vm]?.name ?? "?"
        return info.status == .stopped ? "— \(vm), stopped · ⇧⌘R restarts" : "— \(vm)"
    }

    /// Tahoe gives every toolbar item a glass capsule, which reads as a button the header
    /// is not; without it the item also stops being clipped to a control's width.
    @ToolbarContentBuilder
    private var headerItem: some ToolbarContent {
        if #available(macOS 26.0, *) {
            ToolbarItem(placement: .navigation) { header }
                .sharedBackgroundVisibility(.hidden)
        } else {
            ToolbarItem(placement: .navigation) { header }
        }
    }

    private var header: some View {
        HStack(spacing: 6) {
            if let key = model.selectedKey, let info = model.selectedInfo {
                StatusDot(online: model.isOnline(key.vm),
                          status: info.status,
                          active: model.activeDots.contains(key))
                Text(info.agent)
                    .font(Ghostty.ui(weight: .bold))
                    .foregroundStyle(theme.palette.agent)
                Text(info.name).font(Ghostty.ui())
                Text(subtitle)
                    .font(Ghostty.ui(Ghostty.fontSize - 2))
                    .foregroundStyle(.secondary)
            } else {
                Text("agents-hub").font(Ghostty.ui(weight: .bold)).foregroundStyle(.secondary)
            }
        }
        .fixedSize()
    }

    private var hints: String {
        if model.terminalFocused { return "⌘L back to list" }
        if model.selectionIsFolder { return "j/k move · space fold · f favourite · n new" }
        return "j/k move · ⏎ attach · n new · d kill · / filter"
    }

    private var statusBar: some View {
        HStack(spacing: 10) {
            Text(model.status).lineLimit(1).truncationMode(.middle)
            Spacer()
            // Permanent rather than a status message: a config the terminal silently
            // fell back from is wrong for the whole session, not for a moment.
            if let issue = Ghostty.configIssue {
                Text("ghostty config ignored").foregroundStyle(.orange).help(issue)
            }
            if let info = model.selectedInfo, info.status == .stopped {
                Button("Restart") { model.restartSelected() }.controlSize(.small)
            }
            Text(hints).foregroundStyle(.tertiary)
        }
        .font(Ghostty.ui(Ghostty.fontSize - 2))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }
}

extension View {
    /// The toolbar title only became removable in macOS 15; below that the system-font
    /// label stays and `header` simply repeats it.
    @ViewBuilder
    func withoutToolbarTitle() -> some View {
        if #available(macOS 15.0, *) {
            toolbar(removing: .title)
        } else {
            self
        }
    }
}
