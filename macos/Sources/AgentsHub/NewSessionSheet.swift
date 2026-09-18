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

    private var displayed: [String] { Array(model.cwdMatches(cwd).prefix(50)) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New session on \(model.vms[safe: model.currentVM]?.name ?? "?")")
                .font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))

            LabeledContent("Agent") { agentPicker }

            LabeledContent("Name") {
                TextField(defaultName(cwd: cwd, agent: agent), text: $name)
                    .textFieldStyle(.roundedBorder)
            }

            // Not `LabeledContent`: it centres its label against the menu too, so
            // "Directory" would jump down the sheet whenever the menu appears.
            HStack(alignment: .firstTextBaseline) {
                Text("Directory")
                VStack(alignment: .leading, spacing: 0) {
                    TextField("~/path", text: $cwd)
                        .textFieldStyle(.roundedBorder)
                        .onChange(of: cwd) { _, new in
                            model.requestDirs(new)
                            highlighted = 0
                        }
                        .onSubmit(submit)
                        .onKeyPress(.downArrow) { moveHighlight(1) }
                        .onKeyPress(.upArrow) { moveHighlight(-1) }
                        .onKeyPress(.tab, phases: .down) { press in
                            press.modifiers.isEmpty ? acceptHighlighted() : .ignored
                        }
                    if !displayed.isEmpty {
                        completionMenu
                    }
                }
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
            // A new list under a wheel-scrolled viewport keeps the old offset, with row 0
            // and the highlight above the fold.
            .onChange(of: cwd) { _, _ in
                if let first = displayed.first { proxy.scrollTo(first) }
            }
        }
        // Six rows at the default font size, as CWD_MENU. `fixedSize` last: before the cap
        // it lays the list out at full height and the cap only clips; without it, zero height.
        .frame(maxHeight: 120)
        .fixedSize(horizontal: false, vertical: true)
        .background(.quaternary)
        .cornerRadius(4)
    }

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

    /// Ends on `/` so the menu immediately offers that directory's own children — and so
    /// `cwd` always changes, which is what asks for that listing and resets the highlight.
    private func accept(_ dir: String) {
        cwd = completeDir(cwd, dir)
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
