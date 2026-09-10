import AgentsHubCore
import SwiftUI

/// Agent, name, cwd. The cwd field completes against the VM that will *host* the session,
/// so a remote path completes on the remote box — one `ListDir` per directory, and the
/// reply serves every later keystroke.
struct NewSessionSheet: View {
    @ObservedObject var model: AppModel
    @Environment(\.dismiss) private var dismiss

    @State private var agent = ""
    @State private var name = ""
    @State private var cwd = ""
    @State private var highlighted = 0

    private var matches: [String] { model.cwdMatches(cwd) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New session on \(model.vms[safe: model.currentVM]?.name ?? "?")")
                .font(Ghostty.ui(Ghostty.fontSize + 1, weight: .bold))

            Picker("Agent", selection: $agent) {
                ForEach(model.agentNames, id: \.self) { Text($0).tag($0) }
            }
            .pickerStyle(.segmented)
            .disabled(model.agentNames.isEmpty)

            LabeledContent("Name") {
                TextField(defaultName(cwd: cwd, agent: agent), text: $name)
                    .textFieldStyle(.roundedBorder)
            }

            LabeledContent("Directory") {
                VStack(alignment: .leading, spacing: 0) {
                    TextField("~/path", text: $cwd)
                        .textFieldStyle(.roundedBorder)
                        .onChange(of: cwd) { _, new in
                            model.requestDirs(new)
                            highlighted = 0
                        }
                        .onSubmit(submit)
                    if !matches.isEmpty {
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
    }

    private var completionMenu: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 0) {
                ForEach(Array(matches.prefix(50).enumerated()), id: \.element) { i, dir in
                    Text(dir)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(.horizontal, 6).padding(.vertical, 2)
                        .background(i == highlighted ? Color.accentColor.opacity(0.25) : .clear)
                        .contentShape(Rectangle())
                        .onTapGesture { accept(dir) }
                }
            }
        }
        // Six rows, as CWD_MENU was.
        .frame(maxHeight: 120)
        .background(.quaternary)
        .cornerRadius(4)
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
