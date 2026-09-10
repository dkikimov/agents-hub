import AgentsHubCore
import Foundation
import SwiftUI

struct VMState {
    let config: VMConfig
    var online = false
    var sessions: [SessionInfo] = []

    var name: String { config.name }
}

/// A sidebar row. The id must be stable and content-derived: the list is rebuilt on every
/// `Sessions` frame, and array indices would make the selection jump under the user.
enum SidebarRow: Identifiable, Hashable {
    case folder(vm: Int, path: String, seg: String, depth: Int, hasSub: Bool)
    case elide(vm: Int, path: String, depth: Int)
    case session(vm: Int, id: String, depth: Int)

    var id: String {
        switch self {
        case let .folder(vm, path, _, _, _): return "f/\(vm)/\(path)"
        case let .elide(vm, path, _): return "e/\(vm)/\(path)"
        case let .session(vm, id, _): return "s/\(vm)/\(id)"
        }
    }

    var depth: Int {
        switch self {
        case let .folder(_, _, _, d, _), let .elide(_, _, d), let .session(_, _, d): return d
        }
    }

    var key: SessionKey? {
        if case let .session(vm, id, _) = self { return SessionKey(vm: vm, id: id) }
        return nil
    }
}

struct FoldKey: Hashable, Codable {
    let vm: Int
    let path: String
}

enum Sheet: Identifiable {
    case newSession
    case confirmKill(SessionKey, label: String)

    var id: String {
        switch self {
        case .newSession: return "new"
        case let .confirmKill(k, _): return "kill/\(k.vm)/\(k.id)"
        }
    }
}

/// All client state, plus the queries over it. The twin of `src/tui/app.rs` and the
/// `on_msg` half of `event.rs`.
///
/// Every mutation of the attach set goes through `SessionRegistry`, which returns effects
/// this applies. Nothing else may touch it — that is what keeps the three rules that each
/// cost the Rust client a bug in one testable place.
@MainActor
final class AppModel: ObservableObject {
    @Published private(set) var vms: [VMState] = []
    @Published private(set) var rowsByVM: [[SidebarRow]] = []
    @Published var selection: SidebarRow.ID?
    @Published var filter = "" { didSet { rebuild() } }
    @Published var status = ""
    @Published var sheet: Sheet?
    /// Toggled by ⌘L; the root view watches it and moves focus. A plain signal rather
    /// than reaching into `@FocusState` from the model.
    @Published var requestSidebarFocus = false
    @Published private(set) var mounted: [SessionKey] = []
    /// Who owns the keyboard, as ghostty sees it — SwiftUI's own `@FocusState` doesn't
    /// notice a click landing straight in the surface, and an indicator that lies is
    /// worse than none.
    @Published private(set) var terminalFocused = false
    @Published private(set) var activeDots: Set<SessionKey> = []
    @Published private(set) var configError: String?

    /// Completion listings, keyed by the VM that would host the session and the directory
    /// asked about. One request per directory serves every later keystroke.
    @Published private(set) var dirs: [String: [String]] = [:]

    private(set) var agentNames: [String] = []
    private(set) var terminals: [SessionKey: TerminalSession] = [:]

    private var registry = SessionRegistry()
    private var links: [VMLink] = []
    private let router = TerminalRouter()
    private var collapsed: Set<FoldKey> = []
    private var pane: (cols: UInt16, rows: UInt16) = (80, 24)
    private var dotTimer: Timer?
    private var requestedDirs: Set<String> = []

    /// Matches `mod.rs`'s ACTIVITY_WINDOW.
    private let activityWindow: TimeInterval = 1

    // MARK: - lifecycle

    func start() {
        let config: HubConfig
        do {
            config = try loadConfig()
        } catch {
            configError = error.localizedDescription
            return
        }
        guard !config.vm.isEmpty else {
            configError = "no [[vm]] in config.toml"
            return
        }
        agentNames = config.agentNames
        vms = config.vm.map { VMState(config: $0) }
        collapsed = Self.loadFolds()
        rebuild()

        links = config.vm.enumerated().map { index, vm in
            let link = VMLink(index: index, vm: vm)
            link.onEvent = { [weak self] idx, event in
                self?.handle(idx, event)
            }
            return link
        }
        links.forEach { $0.start() }

        // One timer for every dot, at a quarter of the Rust client's 16 ms frame rate:
        // SwiftUI redraws only what changed, so this only has to be fast enough to look
        // live. It replaces the whole dirty-flag frame loop.
        dotTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshDots() }
        }
    }

    func stop() {
        dotTimer?.invalidate()
        links.forEach { $0.stop() }
    }

    /// `nonisolated` so the link queues can call it; hops to main itself.
    private nonisolated func handle(_ index: Int, _ event: LinkEvent) {
        // PTY bytes never touch the main actor: the router feeds ghostty's own serial
        // queue directly. Everything else is UI-rate and hops.
        if case let .frame(.output(id, data, live)) = event {
            router.deliver(SessionKey(vm: index, id: id), data, live: live)
            return
        }
        Task { @MainActor in self.onMain(index, event) }
    }

    private func onMain(_ index: Int, _ event: LinkEvent) {
        switch event {
        case .up:
            vms[index].online = true
            status = "\(vms[index].name) connected"
            apply(registry.connected(vm: index))
        case let .down(reason):
            vms[index].online = false
            status = reason.map { "\(vms[index].name): \($0)" }
                ?? "\(vms[index].name) offline — retrying"
            apply(registry.disconnected(vm: index))
            rebuild()
        case let .frame(frame):
            onFrame(index, frame)
        }
    }

    private func onFrame(_ index: Int, _ frame: Resp) {
        switch frame {
        case let .sessions(list):
            let previous = allRows
            vms[index].sessions = list
            apply(registry.sessions(vm: index, list, cols: pane.cols, rows: pane.rows))
            rebuild()
            pruneSelection(near: previous)
        case let .exited(id, code):
            let key = SessionKey(vm: index, id: id)
            router.finish(key, code: code)
            if let s = vms[index].sessions.first(where: { $0.id == id }) {
                status = "\(s.name) exited (\(code)) — press r to restart"
            }
        case let .dirs(path, names):
            dirs[Self.dirKey(index, path)] = names
        case let .error(msg):
            status = "\(vms[index].name): \(msg)"
        case .output:
            break  // handled off-main in `handle`
        }
    }

    // MARK: - effects

    private func apply(_ effects: [Effect]) {
        for effect in effects {
            switch effect {
            case let .send(vm, req):
                links[vm].send(req)
            case let .teardown(key):
                makeTerminal(key)
            case let .forget(key):
                router.set(key, nil)
                terminals[key] = nil
                mounted.removeAll { $0 == key }
            }
        }
    }

    private func makeTerminal(_ key: SessionKey) {
        let vm = key.vm
        let terminal = TerminalSession(
            key: key,
            send: { [weak self] req in
                // Off the main actor: this is ghostty's write callback, at keystroke rate.
                Task { @MainActor in self?.links[safe: vm]?.send(req) }
            },
            onGrid: { [weak self] cols, rows in
                Task { @MainActor in self?.gridChanged(key, cols: cols, rows: rows) }
            }
        )
        terminals[key] = terminal
        router.set(key, terminal.session)
        // A restarted session keeps its slot in the ZStack, so the surface is rebuilt in
        // place rather than the pane going blank.
        if mounted.contains(key) {
            terminal.state.isSurfaceVisible = (key == selectedKey)
        }
    }

    // MARK: - selection and mounting

    var selectedKey: SessionKey? {
        guard let selection else { return nil }
        return allRows.first { $0.id == selection }?.key
    }

    var selectedInfo: SessionInfo? {
        guard let key = selectedKey else { return nil }
        return vms[safe: key.vm]?.sessions.first { $0.id == key.id }
    }

    private var allRows: [SidebarRow] { rowsByVM.flatMap { $0 } }

    /// Mount on first selection and never unmount: unmounting destroys the grid and the
    /// scrollback with it, and the 1 MiB pending buffer only refills from that moment.
    /// Hidden surfaces keep parsing, they just stop drawing.
    ///
    /// Deliberately does *not* take keyboard focus. Moving the selection with j/k has to
    /// leave focus in the sidebar, or the second keystroke lands in the agent — the
    /// terminal is entered explicitly, with ⏎ or a click.
    func selectionChanged() {
        guard let key = selectedKey else { return }
        if !mounted.contains(key) { mounted.append(key) }
        for (k, t) in terminals {
            t.state.isSurfaceVisible = (k == key)
        }
    }

    /// Also seeds the first selection: an app that opens onto an empty pane when there
    /// are sessions right there makes you click before it does anything.
    // MARK: - keyboard navigation

    /// Elide rows are inert, exactly as in the Rust client — `j`/`k` step over them
    /// rather than landing on a row that means nothing.
    private var navigableRows: [SidebarRow] {
        allRows.filter { if case .elide = $0 { return false } else { return true } }
    }

    func move(by delta: Int) {
        let rows = navigableRows
        guard !rows.isEmpty else { return }
        let current = rows.firstIndex { $0.id == selection } ?? (delta > 0 ? -1 : rows.count)
        let next = min(max(current + delta, 0), rows.count - 1)
        selection = rows[next].id
        selectionChanged()
    }

    func selectEdge(last: Bool) {
        guard let row = last ? navigableRows.last : navigableRows.first else { return }
        selection = row.id
        selectionChanged()
    }

    /// Folds the folder under the cursor. Selection stays put because collapsing only
    /// ever removes rows *below* it.
    func toggleFoldAtSelection() {
        guard let selection,
              case let .folder(vm, path, _, _, hasSub) = allRows.first(where: { $0.id == selection }),
              hasSub
        else { return }
        toggleFold(vm: vm, path: path)
    }

    func focusSelectedTerminal() {
        guard let key = selectedKey else { return }
        if !mounted.contains(key) { mounted.append(key) }
        terminals[key]?.focus()
    }

    /// A killed session hands the selection to its nearest surviving neighbour, below
    /// first: landing back at the top of the list means scrolling down again after
    /// every kill.
    private func pruneSelection(near previous: [SidebarRow]) {
        if let selection, allRows.contains(where: { $0.id == selection }) { return }
        let survivors = Set(allRows.compactMap { $0.key == nil ? nil : $0.id })
        let neighbour = selection
            .flatMap { id in previous.firstIndex { $0.id == id } }
            .flatMap { i in
                previous[(i + 1)...].first { survivors.contains($0.id) }
                    ?? previous[..<i].last { survivors.contains($0.id) }
            }
        guard let next = neighbour?.id ?? allRows.first(where: { $0.key != nil })?.id else { return }
        selection = next
        selectionChanged()
    }

    // MARK: - geometry

    /// One geometry for every pane, as in the Rust client, but taken from whichever
    /// surface actually laid out a grid rather than derived from view size — the cell
    /// size follows the user's ghostty font and is not ours to guess.
    ///
    /// The reporter already resized itself through its own callback; this is only for
    /// everyone else, who are hidden or unmounted and will never notice the window moved.
    private func gridChanged(_ reporter: SessionKey, cols: UInt16, rows: UInt16) {
        guard cols > 0, rows > 0, (cols, rows) != (pane.cols, pane.rows) else { return }
        pane = (cols, rows)
        let others = registry.resized(cols: cols, rows: rows).filter { effect in
            guard case let .send(vm, .resize(id, _, _)) = effect else { return true }
            return SessionKey(vm: vm, id: id) != reporter
        }
        apply(others)
    }

    // MARK: - commands

    func create(agent: String, name: String, cwd: String) {
        let vm = currentVM
        let name = name.isEmpty ? defaultName(cwd: cwd, agent: agent) : name
        links[safe: vm]?.send(.create(agent: agent, name: name, cwd: cwd,
                                      cols: pane.cols, rows: pane.rows))
        status = "starting \(agent) · \(name)…"
    }

    func kill(_ key: SessionKey) {
        links[safe: key.vm]?.send(.kill(id: key.id))
        status = "killed \(key.id)"
    }

    func restartSelected() {
        guard let info = selectedInfo, info.status == .stopped, let key = selectedKey else { return }
        links[safe: key.vm]?.send(.restart(id: key.id, cols: pane.cols, rows: pane.rows))
        status = "restarting \(info.name)…"
    }

    func killSelected() {
        guard let key = selectedKey, let info = selectedInfo else { return }
        sheet = .confirmKill(key, label: "\(info.agent) · \(info.name)")
    }

    /// The VM a new session would land on: the one holding the selection, else the first.
    var currentVM: Int {
        if let selection, let row = allRows.first(where: { $0.id == selection }) {
            switch row {
            case let .folder(vm, _, _, _, _), let .elide(vm, _, _), let .session(vm, _, _):
                return vm
            }
        }
        return 0
    }

    /// cwd to seed a new session with: the folder you are standing in, the one the
    /// selected session runs in, else this VM's default.
    var newCwd: String {
        if let selection, let row = allRows.first(where: { $0.id == selection }) {
            switch row {
            case let .folder(_, path, _, _, _): return path
            case let .session(vm, id, _):
                if let s = vms[safe: vm]?.sessions.first(where: { $0.id == id }) { return s.cwd }
            case .elide: break
            }
        }
        return vms[safe: currentVM]?.config.isLocal == true
            ? FileManager.default.currentDirectoryPath : "~"
    }

    // MARK: - cwd completion

    func cwdMatches(_ cwd: String) -> [String] {
        guard let (dir, typed) = splitCwd(cwd) else { return [] }
        return filterDirs(dirs[Self.dirKey(currentVM, dir)] ?? [], typed)
    }

    /// Asks the VM that would host the session what lives in the directory being typed.
    /// Only the first ask per directory goes out; the reply serves every later keystroke.
    func requestDirs(_ cwd: String) {
        guard let (dir, _) = splitCwd(cwd) else { return }
        let key = Self.dirKey(currentVM, dir)
        guard !requestedDirs.contains(key) else { return }
        requestedDirs.insert(key)
        links[safe: currentVM]?.send(.listDir(path: dir))
    }

    /// Listings go stale as soon as anything could have changed on the far end.
    func forgetDirCache() {
        dirs.removeAll()
        requestedDirs.removeAll()
    }

    private static func dirKey(_ vm: Int, _ path: String) -> String { "\(vm)\u{0}\(path)" }

    // MARK: - folds and filter

    func isCollapsed(vm: Int, path: String) -> Bool {
        collapsed.contains(FoldKey(vm: vm, path: path))
    }

    func toggleFold(vm: Int, path: String) {
        let key = FoldKey(vm: vm, path: path)
        if collapsed.contains(key) { collapsed.remove(key) } else { collapsed.insert(key) }
        Self.saveFolds(collapsed)
        rebuild()
    }

    private func matches(_ s: SessionInfo) -> Bool {
        guard !filter.isEmpty else { return true }
        let needle = filter.lowercased()
        return s.name.lowercased().contains(needle)
            || s.agent.lowercased().contains(needle)
            || s.cwd.lowercased().contains(needle)
    }

    private func rebuild() {
        rowsByVM = vms.indices.map { vi in
            let sessions = vms[vi].sessions
            let idx = sessions.indices.filter { matches(sessions[$0]) }
            let folds = Set(collapsed.filter { $0.vm == vi }.map(\.path))
            return tree(sessions: sessions, idx: Array(idx), collapsed: folds).map { row in
                switch row.node {
                case let .folder(path, seg, hasSub):
                    return .folder(vm: vi, path: path, seg: seg, depth: row.depth, hasSub: hasSub)
                case let .elide(path):
                    return .elide(vm: vi, path: path, depth: row.depth)
                case let .session(i):
                    return .session(vm: vi, id: sessions[i].id, depth: row.depth)
                }
            }
        }
    }

    /// ponytail: focus rides the dot timer rather than a Combine subscription per
    /// terminal, so it can lag a quarter second. Subscribe to `state.$isFocused` if that
    /// ever shows.
    private func refreshDots() {
        let next = router.active(within: activityWindow)
        if next != activeDots { activeDots = next }
        let focused = selectedKey.flatMap { terminals[$0]?.state.isFocused } ?? false
        if focused != terminalFocused { terminalFocused = focused }
    }

    func info(for key: SessionKey) -> SessionInfo? {
        vms[safe: key.vm]?.sessions.first { $0.id == key.id }
    }

    func isOnline(_ vm: Int) -> Bool { vms[safe: vm]?.online ?? false }

    // MARK: - fold persistence

    // Folds were in-memory only in the Rust client and reset on every launch, which was
    // marked as debt there. Two lines here.
    private static let foldsKey = "collapsedFolders"

    private static func loadFolds() -> Set<FoldKey> {
        guard let data = UserDefaults.standard.data(forKey: foldsKey),
              let folds = try? JSONDecoder().decode(Set<FoldKey>.self, from: data)
        else { return [] }
        return folds
    }

    private static func saveFolds(_ folds: Set<FoldKey>) {
        guard let data = try? JSONEncoder().encode(folds) else { return }
        UserDefaults.standard.set(data, forKey: foldsKey)
    }
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
