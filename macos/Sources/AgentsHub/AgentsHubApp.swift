import AgentsHubCore
import SwiftUI

@main
struct AgentsHubApp: App {
    @StateObject private var model = AppModel()
    @AppStorage("appearance") private var appearance = AppAppearance.dark

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
            }
        }
    }
}

struct RootView: View {
    @ObservedObject var model: AppModel
    let appearance: AppAppearance

    @Environment(\.colorScheme) private var systemScheme
    @FocusState private var focus: FocusTarget?

    private var scheme: ColorScheme { appearance.colorScheme ?? systemScheme }

    var body: some View {
        NavigationSplitView {
            SidebarView(model: model, focus: $focus)
                .navigationSplitViewColumnWidth(min: 190, ideal: 270, max: 440)
        } detail: {
            VStack(spacing: 0) {
                TerminalPane(model: model)
                    .focused($focus, equals: .terminal)
                Divider()
                statusBar
            }
            .navigationTitle(title)
            .navigationSubtitle(subtitle)
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
        return info.status == .stopped ? "\(vm) — stopped, ⇧⌘R to restart" : vm
    }

    private var statusBar: some View {
        HStack(spacing: 10) {
            Text(model.status).lineLimit(1).truncationMode(.middle)
            Spacer()
            if let info = model.selectedInfo, info.status == .stopped {
                Button("Restart") { model.restartSelected() }.controlSize(.small)
            }
            Text(focus == .terminal ? "⌘L back to list" : "j/k move · ⏎ attach · / filter")
                .foregroundStyle(.tertiary)
        }
        .font(.system(size: 11))
        .foregroundStyle(.secondary)
        .padding(.horizontal, 10)
        .padding(.vertical, 4)
    }
}
