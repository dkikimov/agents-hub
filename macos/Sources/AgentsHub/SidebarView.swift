import AgentsHubCore
import SwiftUI

enum FocusTarget: Hashable {
    case sidebar
    case terminal
}

/// vim keys, live only while the sidebar has focus — once focus is in the terminal every
/// keystroke belongs to the agent, which is the whole reason the TUI needed a `Ctrl-]`
/// escape chord and this does not.
private struct VimKeys: ViewModifier {
    @ObservedObject var model: AppModel
    @FocusState.Binding var focus: FocusTarget?
    @FocusState.Binding var filterFocused: Bool

    func body(content: Content) -> some View {
        content
            .onKeyPress("j") { model.move(by: 1); return .handled }
            .onKeyPress("k") { model.move(by: -1); return .handled }
            .onKeyPress("g") { model.selectEdge(last: false); return .handled }
            .onKeyPress("G") { model.selectEdge(last: true); return .handled }
            .onKeyPress(.space) { model.toggleFoldAtSelection(); return .handled }
            .onKeyPress("l") { enterTerminal() }
            .onKeyPress(.rightArrow) { enterTerminal() }
            .onKeyPress(.return) { enterTerminal() }
            .onKeyPress("/") { filterFocused = true; return .handled }
            .onKeyPress("n") { model.sheet = .newSession; return .handled }
            .onKeyPress("d") { model.killSelected(); return .handled }
            .onKeyPress("r") { model.restartSelected(); return .handled }
    }

    private func enterTerminal() -> KeyPress.Result {
        guard model.selectedKey != nil else { return .ignored }
        focus = .terminal
        model.focusSelectedTerminal()
        return .handled
    }
}

/// The status glyph, ported from `render.rs::session_marker`. The one thing worth copying
/// exactly, because it is how you tell at a glance which agent wants you.
struct StatusDot: View {
    let online: Bool
    let status: Status
    let active: Bool

    var body: some View {
        Text(glyph).foregroundStyle(color).font(Ghostty.ui(Ghostty.fontSize - 2))
    }

    private var glyph: String {
        if !online { return "◌" }
        if status == .stopped { return "○" }
        return active ? "◉" : "●"
    }

    private var color: Color {
        if !online || status == .stopped { return .secondary }
        return active ? .yellow : .green
    }
}

struct SidebarView: View {
    @ObservedObject var model: AppModel
    @FocusState.Binding var focus: FocusTarget?
    @FocusState private var filterFocused: Bool

    var body: some View {
        VStack(spacing: 0) {
            List(selection: $model.selection) {
                ForEach(model.vms.indices, id: \.self) { vi in
                    Section {
                        ForEach(model.rowsByVM[safe: vi] ?? []) { row in
                            rowView(row).tag(row.id)
                        }
                    } header: {
                        HStack(spacing: 6) {
                            Text(model.vms[vi].name)
                                .font(Ghostty.ui(Ghostty.fontSize - 1, weight: .bold))
                                .foregroundStyle(.cyan)
                            if !model.vms[vi].online {
                                Text("offline")
                                    .font(Ghostty.ui(Ghostty.fontSize - 3))
                                    .foregroundStyle(.yellow)
                            }
                        }
                    }
                }
            }
            .listStyle(.sidebar)
            .focused($focus, equals: .sidebar)
            .onChange(of: model.selection) { _, _ in model.selectionChanged() }
            .modifier(VimKeys(model: model, focus: $focus, filterFocused: $filterFocused))

            Divider()
            HStack(spacing: 6) {
                Image(systemName: "line.3.horizontal.decrease").foregroundStyle(.secondary)
                TextField("filter", text: $model.filter)
                    .textFieldStyle(.plain)
                    .font(Ghostty.ui(Ghostty.fontSize - 1))
                    .focused($filterFocused)
                    .onSubmit { focus = .sidebar }
                    .onExitCommand { model.filter = ""; focus = .sidebar }
                if !model.filter.isEmpty {
                    Button { model.filter = "" } label: { Image(systemName: "xmark.circle.fill") }
                        .buttonStyle(.plain).foregroundStyle(.secondary)
                }
            }
            .padding(.horizontal, 8).padding(.vertical, 5)
        }
    }

    @ViewBuilder
    private func rowView(_ row: SidebarRow) -> some View {
        switch row {
        case let .folder(vm, path, seg, depth, hasSub):
            HStack(spacing: 4) {
                indent(depth)
                if hasSub {
                    Button {
                        model.toggleFold(vm: vm, path: path)
                    } label: {
                        Image(systemName: model.isCollapsed(vm: vm, path: path)
                              ? "chevron.right" : "chevron.down")
                            .font(.system(size: 9))
                    }
                    .buttonStyle(.plain).foregroundStyle(.secondary)
                } else {
                    Text("·").foregroundStyle(.secondary).font(Ghostty.ui())
                }
                Text(seg).foregroundStyle(.blue).font(Ghostty.ui())
            }

        case let .elide(_, _, depth):
            HStack(spacing: 4) {
                indent(depth)
                Text("…").foregroundStyle(.secondary).font(Ghostty.ui())
            }
            // Deletes App::step's skip-the-inert-row loop: the list just won't land here.
            .selectionDisabled()

        case let .session(vm, id, depth):
            let key = SessionKey(vm: vm, id: id)
            let info = model.info(for: key)
            HStack(spacing: 5) {
                indent(depth)
                StatusDot(online: model.isOnline(vm),
                          status: info?.status ?? .stopped,
                          active: model.activeDots.contains(key))
                Text(info?.agent ?? "?").foregroundStyle(.purple).font(Ghostty.ui())
                Text(info?.name ?? id)
                    .font(Ghostty.ui())
                    .lineLimit(1).truncationMode(.tail)
            }
        }
    }

    /// List's own indentation is for OutlineGroup, which cannot express the collapse rule
    /// this tree uses, so it is done by hand.
    private func indent(_ depth: Int) -> some View {
        Color.clear.frame(width: CGFloat(depth) * 11, height: 1)
    }
}
