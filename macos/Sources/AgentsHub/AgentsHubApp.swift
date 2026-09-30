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

    /// SwiftUI does not rebuild a `WindowGroup` window once the last one is closed, so a Dock
    /// click on the still-running app does nothing at all. The scene hands its `openWindow`
    /// over here for exactly that case.
    @MainActor static var reopen: (() -> Void)?

    /// A backgrounded tab still sizes its ghostty surface from the window it cannot see, so
    /// tabbed panes come back at a size nothing asked for. Set before the first window exists,
    /// which is what also removes ⌘T and the Window > Tab menu section.
    func applicationWillFinishLaunching(_ notification: Notification) {
        NSWindow.allowsAutomaticWindowTabbing = false
    }

    /// A miniaturized window counts as not visible too, but AppKit restores that one itself —
    /// opening on top of it would leave the user with two.
    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows, !sender.windows.contains(where: { $0.isMiniaturized }) {
            Task { @MainActor in Self.reopen?() }
        }
        return true
    }

    func application(_ sender: NSApplication, open urls: [URL]) {
        let paths = urls.filter(\.hasDirectoryPath).map(\.path)
        Task { @MainActor in
            if let onOpen = Self.onOpen { onOpen(paths) } else { Self.pending += paths }
        }
    }
}

@main
struct AgentsHubApp: App {
    static let mainWindow = "main"

    @StateObject private var model = AppModel()
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @AppStorage("appearance") private var appearance = AppAppearance.dark
    @AppStorage("theme") private var theme = Theme.classic

    var body: some Scene {
        WindowGroup(id: Self.mainWindow) {
            RootView(model: model, appearance: appearance)
                .task {
                    // Launched from a shell rather than Finder, the app otherwise opens
                    // behind whatever is frontmost. Never `ignoringOtherApps` — that
                    // steals keystrokes mid-sentence into whichever session is attached.
                    NSApp.activate()
                    model.start()
                }
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
                // A shell in the selected session's cwd, on its VM, run by that VM's daemon.
                // Must be a key ghostty leaves unbound (`ghostty +list-keybinds --default`),
                // or a focused surface eats it before the menu: ⌘J is scroll_to_selection.
                Button("Toggle Terminal") { model.toggleShell() }
                    .keyboardShortcut("b", modifiers: .command)
                Button("New Terminal") { model.newShell() }
                    .keyboardShortcut("t", modifiers: .command)
                Menu("Switch Terminal") {
                    ForEach(1...9, id: \.self) { n in
                        Button("Terminal \(n)") { model.selectShell(at: n - 1) }
                            .keyboardShortcut(KeyEquivalent(Character("\(n)")), modifiers: .command)
                    }
                }
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
    @AppStorage(ShellPanelSize.key) private var shellFraction = ShellPanelSize.initial

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
            LabeledContent("Terminal panel (⌘B)") {
                HStack {
                    Slider(value: $shellFraction, in: ShellPanelSize.range)
                    Text("\(Int((shellFraction * 100).rounded()))%")
                        .monospacedDigit()
                        .frame(width: 40, alignment: .trailing)
                }
            }
            Text("Height of the shell panel, as a share of the terminal area. Dragging the "
                 + "panel's header sets the same value.")
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
    @Environment(\.openWindow) private var openWindow
    @FocusState private var focus: FocusTarget?
    @AppStorage("theme") private var theme = Theme.classic
    @AppStorage("sidebarWidth") private var sidebarWidth = 270.0
    @State private var widthAtDragStart: Double?
    @AppStorage(ShellPanelSize.key) private var shellFraction = ShellPanelSize.initial
    @State private var fractionAtDragStart: Double?

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
                terminals
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
            AppDelegate.reopen = { openWindow(id: AgentsHubApp.mainWindow) }
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

    /// The agent pane, with the ⌘B shell panel under it when the selected session has one
    /// open.
    ///
    /// The panel is laid out at its full height even while closed, behind the agent pane
    /// at `opacity(0)`: pulling it out of the hierarchy would destroy every shell's
    /// scrollback, and a zero-height one would build no surface at all — the same traps
    /// `TerminalPane` documents. Opening it only shrinks the agent pane to uncover it.
    private var terminals: some View {
        GeometryReader { geo in
            let panel = (geo.size.height * CGFloat(ShellPanelSize.clamp(shellFraction))).rounded()
            ZStack(alignment: .bottom) {
                ShellPane(model: model)
                    .frame(height: panel)
                    .opacity(model.shellPanelOpen ? 1 : 0)
                    .allowsHitTesting(model.shellPanelOpen)
                VStack(spacing: 0) {
                    TerminalPane(model: model)
                        .focused($focus, equals: .terminal)
                    if model.shellPanelOpen {
                        shellHandle(total: geo.size.height)
                        Color.clear
                            .frame(height: panel)
                            .allowsHitTesting(false)
                    }
                }
            }
        }
    }

    /// The panel's header, which is also its resize handle. Global coordinates, because
    /// the handle moves with the drag and a local translation would chase its own tail.
    private func shellHandle(total: CGFloat) -> some View {
        VStack(spacing: 0) {
            Divider()
            HStack {
                ShellTabs(model: model)
                Spacer()
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
        }
        .background(Ghostty.background)
        .contentShape(.rect)
        .onHover { $0 ? NSCursor.resizeUpDown.push() : NSCursor.pop() }
        .gesture(
            DragGesture(minimumDistance: 1, coordinateSpace: .global)
                .onChanged { drag in
                    let start = fractionAtDragStart ?? shellFraction
                    fractionAtDragStart = start
                    let moved = Double(drag.translation.height / max(total, 1))
                    shellFraction = ShellPanelSize.clamp(start - moved)
                }
                .onEnded { _ in fractionAtDragStart = nil }
        )
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
        if model.terminalFocused { return "⌘L back to list · ⌘B shell · ⌘T new · ⌘1-9 switch" }
        if model.selectionIsFolder { return "j/k move · space fold · f favourite · n new" }
        return "j/k move · ⏎ attach · ⌘B shell · n new · d kill · / filter"
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
