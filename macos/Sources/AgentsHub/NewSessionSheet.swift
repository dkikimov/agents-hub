import AgentsHubCore
import SwiftUI

/// Agent, name, cwd. The cwd field completes against the VM that will *host* the session,
/// so a remote path completes on the remote box — one `ListDir` per directory, and the
/// reply serves every later keystroke.
struct NewSessionSheet: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss
    @AppStorage("theme") private var theme = Theme.classic

    @State private var agent = ""
    @State private var name = ""
    @State private var cwd = ""
    @State private var highlighted = 0

    private var matches: [String] { model.cwdMatches(cwd) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New session on \(model.vms[safe: model.currentVM]?.name ?? "?")")
                .font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))

            LabeledContent("Agent") { agentPicker }

            LabeledContent("Name") {
                TextField(defaultName(cwd: cwd, agent: agent), text: $name)
                    .textFieldStyle(.roundedBorder)
            }

            LabeledContent("Directory") {
                TextField("~/path", text: $cwd)
                    .textFieldStyle(.roundedBorder)
                    .onChange(of: cwd) { _, new in
                        model.requestDirs(new)
                        highlighted = 0
                    }
                    .onSubmit(submit)
                    .onKeyPress(.downArrow) { moveHighlight(1) }
                    .onKeyPress(.upArrow) { moveHighlight(-1) }
                    .onKeyPress(.tab) { acceptHighlighted() }
            }

            // A row of its own, not part of the `LabeledContent` above: that row proposes a
            // single line's height to its value view, and a `ScrollView` handed that
            // collapses to nothing — the menu was in the view tree the whole time, zero
            // points tall, which is why Tab completed against a list nobody could see.
            if !matches.isEmpty {
                completionMenu
            }

            HStack {
                if model.agentNames.isEmpty {
                    Text("no [agents.*] in config.toml").foregroundStyle(.red).font(Ghostty.ui(Ghostty.fontSize - 2))
                }
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Start", action: submit)
                    .keyboardShortcut(.defaultAction)
                    .disabled(agent.isEmpty)
            }
        }
        .padding(16)
        .frame(width: 520)
        .font(Ghostty.ui())
        .onAppear {
            agent = model.agentNames.first ?? ""
            cwd = model.newCwd
            model.forgetDirCache()
            model.requestDirs(cwd)
        }
        // Both Start and Cancel land here, and `submit` has already read the dropped VM by
        // the time it dismisses.
        .onDisappear { model.clearDrop() }
    }

    /// Command-digit rather than a segmented `Picker`: an NSSegmentedControl is reachable
    /// only by mouse unless Full Keyboard Access is on, and this modal has to be typeable.
    private var agentPicker: some View {
        HStack(spacing: 4) {
            ForEach(Array(model.agentNames.enumerated()), id: \.element) { i, name in
                Button { agent = name } label: { segment(name, index: i) }
                    .buttonStyle(.plain)
                    .keyboardShortcut(shortcut(i))
            }
            Spacer()
        }
    }

    private func segment(_ name: String, index: Int) -> some View {
        HStack(spacing: 5) {
            Text(name)
            if shortcut(index) != nil {
                Text("⌘\(index + 1)")
                    .font(Ghostty.ui(Ghostty.fontSize - 3))
                    .foregroundStyle(.secondary)
            }
        }
        .padding(.horizontal, 8).padding(.vertical, 3)
        .background(name == agent ? theme.palette.accent.opacity(0.35) : Color.secondary.opacity(0.15))
        .cornerRadius(4)
        .contentShape(Rectangle())
    }

    private func shortcut(_ index: Int) -> KeyboardShortcut? {
        guard index < 9 else { return nil }
        return KeyboardShortcut(KeyEquivalent(Character("\(index + 1)")), modifiers: .command)
    }

    private var completionMenu: some View {
        ScrollViewReader { proxy in
            ScrollView {
                VStack(alignment: .leading, spacing: 0) {
                    ForEach(Array(displayed.enumerated()), id: \.element) { i, dir in
                        Text(dir)
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 6).padding(.vertical, 2)
                            .background(i == highlighted ? theme.palette.accent.opacity(0.25) : .clear)
                            .contentShape(Rectangle())
                            .onTapGesture { accept(dir) }
                            .id(dir)
                    }
                }
            }
            .onChange(of: highlighted) { _, i in
                guard let dir = displayed[safe: i] else { return }
                proxy.scrollTo(dir)
            }
        }
        // Six rows, as CWD_MENU was — but sized to the rows it actually has first, or the
        // ScrollView has no height of its own to clamp.
        .fixedSize(horizontal: false, vertical: true)
        .frame(maxHeight: 120)
        .background(.quaternary)
        .cornerRadius(4)
    }

    private var displayed: [String] { Array(matches.prefix(50)) }

    private func moveHighlight(_ delta: Int) -> KeyPress.Result {
        guard !displayed.isEmpty else { return .ignored }
        highlighted = min(max(highlighted + delta, 0), displayed.count - 1)
        return .handled
    }

    private func acceptHighlighted() -> KeyPress.Result {
        guard let dir = displayed[safe: highlighted] else { return .ignored }
        accept(dir)
        return .handled
    }

    /// Ends on `/` so the menu immediately offers that directory's own children.
    private func accept(_ dir: String) {
        cwd = completeDir(cwd, dir)
        model.requestDirs(cwd)
        highlighted = 0
    }

    private func submit() {
        guard !agent.isEmpty else { return }
        model.create(agent: agent, name: name, cwd: cwd)
        dismiss()
    }
}

struct KillConfirm: View {
    @ObservedObject var model: AppModel
    let key: SessionKey
    let label: String
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Kill \(label)?").font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))
            // The daemon deletes the log too, so this really is irreversible.
            Text("Its scrollback is deleted with it. This cannot be undone.")
                .font(Ghostty.ui(Ghostty.fontSize - 2)).foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }.keyboardShortcut(.cancelAction)
                Button("Kill", role: .destructive) {
                    model.kill(key)
                    dismiss()
                }
                .keyboardShortcut(.defaultAction)
            }
        }
        .padding(16)
        .frame(width: 380)
        .font(Ghostty.ui())
    }
}
