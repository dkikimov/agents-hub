import AgentsHubCore
import SwiftUI

@main
struct AgentsHubApp: App {
    @StateObject private var model = AppModel()
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

    private var scheme: ColorScheme { appearance.colorScheme ?? systemScheme }

    var body: some View {
        NavigationSplitView {
            SidebarView(model: model, focus: $focus)
                .navigationSplitViewColumnWidth(min: 190, ideal: 270, max: 440)
        } detail: {
            VStack(spacing: 0) {
                header
                Divider()
                TerminalPane(model: model)
                    .focused($focus, equals: .terminal)
                    .focusRing(model.terminalFocused ? theme.palette.accent : .clear)
                Divider()
                statusBar
            }
            // No `navigationTitle`: NavigationSplitView draws it into the toolbar in the
            // system font, which is the one seam you notice above a monospaced UI.
            // `header` carries the same information in the terminal's own font.
            .withoutToolbarTitle()
        }
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

    private var title: String {
        guard let info = model.selectedInfo else { return "agents-hub" }
        return "\(info.agent) · \(info.name)"
    }

    private var subtitle: String {
        guard let key = model.selectedKey, let info = model.selectedInfo else { return "" }
        let vm = model.vms[safe: key.vm]?.name ?? "?"
        return info.status == .stopped ? "— \(vm), stopped · ⇧⌘R restarts" : "— \(vm)"
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
            Spacer()
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 6)
    }

    private var statusBar: some View {
        HStack(spacing: 10) {
            Text(model.status).lineLimit(1).truncationMode(.middle)
            Spacer()
            if let info = model.selectedInfo, info.status == .stopped {
                Button("Restart") { model.restartSelected() }.controlSize(.small)
            }
            Text(model.terminalFocused ? "⌘L back to list" : "j/k move · ⏎ attach · / filter")
                .foregroundStyle(.tertiary)
        }
        .font(Ghostty.ui(Ghostty.fontSize - 2))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }
}

extension View {
    /// Only the terminal is outlined, and only while it holds the keyboard: the sidebar
    /// already says so with its own selection highlight, and its column runs into the
    /// window's rounded corners, where a square stroke looks wrong.
    func focusRing(_ color: Color) -> some View {
        overlay(
            Rectangle()
                .strokeBorder(color, lineWidth: 2)
                .allowsHitTesting(false)
        )
    }

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
