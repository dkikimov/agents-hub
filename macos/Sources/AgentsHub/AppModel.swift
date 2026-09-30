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

struct PathKey: Hashable, Codable {
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
    /// Agent sessions whose ⌘B shell panel is open. Per session, so switching to one
    /// without a shell does not open an empty panel under it.
    @Published private(set) var shellOpen: Set<SessionKey> = []
    /// Companion shells with a surface in the panel. Same never-unmount rule as `mounted`,
    /// for the same reason: the surface *is* the scrollback.
    @Published private(set) var mountedShells: [SessionKey] = []
    /// The panel tab each agent session last showed. A missing or dead entry falls back
    /// to its first shell.
    @Published private(set) var activeShells: [SessionKey: SessionKey] = [:]
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
    private var bells = BellWatch()
    private let notifier = Notifier()
    private var collapsed: Set<PathKey> = []
    private var favourites: Set<PathKey> = []
    private var pane = AppModel.loadGrid(AppModel.paneGridKey) ?? (80, 24)
    /// The panel's own grid, reported by whichever shell surface laid out last. Only the
    /// size a brand-new shell starts at; a mounted one resizes itself.
    private var shellGrid = AppModel.loadGrid(AppModel.shellGridKey) ?? (80, 12)
    /// The session whose shell ⌘B just asked for, so it takes the keyboard the moment it
    /// has a live surface — which, for a first open, is a round trip to the daemon later.
    private var shellFocusPending: SessionKey?
    /// A new tab asked of the daemon, and the shells its parent had before — the one not
    /// among them when it shows up is the tab to switch to.
    private var awaitingShell: (parent: SessionKey, known: Set<String>)?
    private var dotTimer: Timer?
    private var requestedDirs: Set<String> = []
    private var windowVisible = true
    private var windowShown = false
    private weak var paneWindow: NSWindow?

    /// Set only while the New Session sheet is showing a dropped folder, and cleared when it
    /// closes. It overrides the VM the selection implies, so cwd completion and `create` both
    /// target the machine the path is actually on — every reader of `currentVM` is
    /// sheet-scoped, which is what makes overriding it safe.
    private var drop: (cwd: String, vm: Int?)?

    /// Matches `mod.rs`'s ACTIVITY_WINDOW.
    private let activityWindow: TimeInterval = 1

    // MARK: - lifecycle

    /// Reopening a closed window runs the scene's `.task` again against the same model,
    /// so this has to be a no-op the second time rather than a second set of links.
    func start() {
        guard links.isEmpty else { return }
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
        collapsed = Self.load(Self.foldsKey)
        favourites = Self.load(Self.favouritesKey)
        rebuild()
        notifier.start { [weak self] reason in self?.status = reason }

        links = config.vm.enumerated().map { index, vm in
            let link = VMLink(index: index, vm: vm)
            link.onEvent = { [weak self] idx, event in
                self?.handle(idx, event)
            }
            return link
        }
        links.forEach { $0.start() }

        // Claimed here rather than from the scene's `.task`, so a folder can never arrive
        // before there are VMs to route it to.
        AppDelegate.onOpen = { [weak self] paths in self?.openFolders(paths) }
        openFolders(AppDelegate.pending)
        AppDelegate.pending = []

        // One timer for every dot, at a quarter of the Rust client's 16 ms frame rate:
        // SwiftUI redraws only what changed, so this only has to be fast enough to look
        // live. It replaces the whole dirty-flag frame loop.
        dotTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.refreshDots() }
        }
        // Nothing here is deadline work, so let the OS coalesce the wakeup with whatever
        // else it was going to run — four exact timer fires a second is pure idle energy.
        dotTimer?.tolerance = 0.1
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
            apply(registry.sessions(vm: index, list, cols: pane.cols, rows: pane.rows,
                                    shellCols: shellGrid.cols, shellRows: shellGrid.rows))
            rebuild()
            pruneSelection(near: previous)
            adoptNewShell()
            revealShell()
        case let .exited(id, code):
            let key = SessionKey(vm: index, id: id)
            router.finish(key, code: code)
            if let s = vms[index].sessions.first(where: { $0.id == id }) {
                if s.isShell {
                    // `exit` closes its tab, as in any editor's terminal.
                    closeShellTab(key)
                    status = "shell exited (\(code))"
                } else {
                    status = "\(s.name) exited (\(code)) — press r to restart"
                }
            }
        case let .dirs(path, names):
            dirs[Self.dirKey(index, path)] = names
        case let .error(msg):
            status = "\(vms[index].name): \(msg)"
            // Most likely a daemon that predates `Shell` and has no idea what we asked
            // for. Leaving the panel open would be a blank promise that never resolves.
            if let parent = shellFocusPending, parent.vm == index, shellKeys(for: parent).isEmpty {
                closeShell(parent)
            }
        case .output:
            break  // handled off-main in `handle`
        }
    }

    // MARK: - effects

    /// The one way out. Every request aimed at a session is also a poke that will make the
    /// agent print — a focus report, a SIGWINCH from `attach` or `resize`, an echoed
    /// keystroke — and `ActivityWatch` needs to know so it doesn't read our own nudge as
    /// the agent working. A second exit would be a dot that lies again.
    private func send(_ vm: Int, _ req: Req) {
        switch req {
        case let .input(id, _), let .resize(id, _, _),
             let .attach(id, _, _), let .restart(id, _, _):
            router.poked(SessionKey(vm: vm, id: id))
        default:
            break
        }
        links[safe: vm]?.send(req)
    }

    private func apply(_ effects: [Effect]) {
        for effect in effects {
            switch effect {
            case let .send(vm, req):
                send(vm, req)
            case let .teardown(key):
                makeTerminal(key)
            case let .forget(key):
                router.set(key, nil)
                terminals[key] = nil
                bells.forget(key)
                mounted.removeAll { $0 == key }
                mountedShells.removeAll { $0 == key }
                shellOpen.remove(key)
                activeShells[key] = nil
                if shellFocusPending == key { shellFocusPending = nil }
                if awaitingShell?.parent == key { awaitingShell = nil }
            }
        }
    }

    /// Mounted at once, selected or not: until a surface exists its bytes wait in the
    /// feed's backlog, and a busy agent overflows that in minutes — trimming the mode
    /// prelude and leaving a garbled screen that pastes unbracketed. A hidden surface
    /// costs memory, not draws.
    ///
    /// A focused terminal hands the keyboard to its replacement; the new surface is a new
    /// view, so first-responder does not carry over on its own.
    private func makeTerminal(_ key: SessionKey) {
        let vm = key.vm
        let hadFocus = terminals[key]?.state.isFocused ?? false
        let terminal = TerminalSession(
            key: key,
            send: { [weak self, router] req in
                // Off the main actor: this is ghostty's write callback, at keystroke rate.
                // The poke is taken here rather than after the hop, because a main actor
                // busy with a window drag is exactly when this fires and the dot has to
                // know about the nudge before the reply to it lands.
                router.poked(key)
                Task { @MainActor in self?.send(vm, req) }
            },
            onGrid: { [weak self] cols, rows in
                Task { @MainActor in self?.gridChanged(key, cols: cols, rows: rows) }
            }
        )
        terminals[key] = terminal
        router.set(key, terminal.feed)
        if info(for: key)?.isShell == true {
            if !mountedShells.contains(key) { mountedShells.append(key) }
        } else if !mounted.contains(key) {
            mounted.append(key)
        }
        applySurfaceVisibility()
        if hadFocus { terminal.focus() }
    }

    /// Closing the window frees every surface with it, and the one the reopened window
    /// builds starts from a feed long since drained. Only the daemon still has the
    /// history, so every link reconnects and the replay rebuilds each terminal.
    func windowAppeared() {
        defer { windowShown = true }
        guard windowShown else { return }
        links.forEach { $0.reconnect() }
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

    /// Folder rows and session rows answer to different keys, so the hint line says which.
    var selectionIsFolder: Bool {
        guard let selection, case .folder = allRows.first(where: { $0.id == selection })
        else { return false }
        return true
    }

    /// Never unmount: unmounting destroys the grid and the scrollback with it. Hidden
    /// surfaces keep parsing, they just stop drawing.
    ///
    /// Deliberately does *not* take keyboard focus. Moving the selection with j/k has to
    /// leave focus in the sidebar, or the second keystroke lands in the agent — the
    /// terminal is entered explicitly, with ⏎ or a click.
    func selectionChanged() {
        applySurfaceVisibility()
        revealShell()
    }

    func trackWindow(_ window: NSWindow?) {
        paneWindow = window
        refreshWindowVisibility()
    }

    /// Occlusion, not app activation: an agent you watch while typing in another app has
    /// to keep drawing. Only a window that is genuinely off-screen — minimized, fully
    /// covered, on another Space — stops its surfaces, and nothing else on macOS does it:
    /// the package's `setApplicationActive` has a UIKit caller only, so without this the
    /// display link runs at up to 120 Hz for a pane nobody can see.
    ///
    /// Polled rather than driven by `didChangeOcclusionState` alone, because AppKit posts
    /// nothing when the window is first ordered in: the only reading available when the
    /// view attaches is the one from before the window was on screen, and trusting it
    /// leaves every pane dark for the session. No window yet means draw — a surface with
    /// nowhere to draw is already the package's own stop condition.
    private func refreshWindowVisibility() {
        let visible = paneWindow.map { $0.occlusionState.contains(.visible) } ?? true
        guard windowVisible != visible else { return }
        windowVisible = visible
        applySurfaceVisibility()
    }

    /// The selected session draws, plus its shell when the panel is open, and only while
    /// the window is on screen. Hidden surfaces keep parsing — that is what still rings
    /// their bells.
    private func applySurfaceVisibility() {
        let drawing: Set<SessionKey> = windowVisible
            ? Set([selectedKey, visibleShellKey].compactMap { $0 })
            : []
        for (key, terminal) in terminals
        where terminal.state.isSurfaceVisible != drawing.contains(key) {
            terminal.state.isSurfaceVisible = drawing.contains(key)
        }
    }

    // MARK: - companion shell

    /// The shells that belong to `parent`, oldest first as the daemon lists them.
    func shellKeys(for parent: SessionKey) -> [SessionKey] {
        (vms[safe: parent.vm]?.sessions ?? [])
            .filter { $0.parent == parent.id }
            .map { SessionKey(vm: parent.vm, id: $0.id) }
    }

    var shellPanelOpen: Bool { selectedKey.map { shellOpen.contains($0) } ?? false }

    var visibleShellKey: SessionKey? {
        guard let key = selectedKey, shellOpen.contains(key) else { return nil }
        return activeShell(of: key)
    }

    private func activeShell(of parent: SessionKey) -> SessionKey? {
        let shells = shellKeys(for: parent)
        return activeShells[parent].flatMap { shells.contains($0) ? $0 : nil } ?? shells.first
    }

    private var selectedAgent: SessionKey? {
        guard let key = selectedKey, info(for: key)?.isShell == false else { return nil }
        return key
    }

    /// ⌘B. Opening with no shell yet starts one; otherwise it shows the last tab.
    func toggleShell() {
        guard let key = selectedAgent else { return }
        if shellOpen.contains(key) {
            closeShell(key)
        } else if let shell = activeShell(of: key) {
            show(shell, of: key, focus: true)
        } else {
            newShell()
        }
    }

    /// ⌘T and the panel's +: another shell in the session's cwd, which takes the tab and
    /// the keyboard once the daemon has made it.
    func newShell() {
        guard let key = selectedAgent else { return }
        shellOpen.insert(key)
        shellFocusPending = key
        awaitingShell = (key, Set(shellKeys(for: key).map(\.id)))
        send(key.vm, .shell(parent: key.id, cols: shellGrid.cols, rows: shellGrid.rows, new: true))
        applySurfaceVisibility()
    }

    /// ⌘1…⌘9: the selected session's shells in tab order.
    func selectShell(at index: Int) {
        guard let key = selectedAgent, let shell = shellKeys(for: key)[safe: index] else { return }
        show(shell, of: key, focus: true)
    }

    /// A tab click.
    func showShell(_ shell: SessionKey) {
        guard let parent = info(for: shell)?.parent else { return }
        show(shell, of: SessionKey(vm: shell.vm, id: parent), focus: true)
    }

    /// A stopped tab is relaunched on sight — after a daemon restart every one is.
    private func show(_ shell: SessionKey, of parent: SessionKey, focus: Bool) {
        activeShells[parent] = shell
        shellOpen.insert(parent)
        if focus { shellFocusPending = parent }
        if info(for: shell)?.status == .stopped {
            send(shell.vm, .restart(id: shell.id, cols: shellGrid.cols, rows: shellGrid.rows))
        }
        applySurfaceVisibility()
        revealShell()
    }

    /// The tab's ×, and a shell's own `exit`. The panel closes with its last tab, and the
    /// keyboard follows the tab that takes the closed one's place only if it was there.
    func closeShellTab(_ shell: SessionKey) {
        guard let parentID = info(for: shell)?.parent else { return }
        let parent = SessionKey(vm: shell.vm, id: parentID)
        let shells = shellKeys(for: parent)
        send(shell.vm, .kill(id: shell.id))
        let rest = shells.filter { $0 != shell }
        guard let fallback = rest[safe: min(shells.firstIndex(of: shell) ?? 0, rest.count - 1)]
        else { return closeShell(parent) }
        guard activeShell(of: parent) == shell else { return }
        let hadFocus = terminals[shell]?.state.isFocused ?? false
        show(fallback, of: parent, focus: hadFocus)
    }

    /// Focus goes back to the agent only if it was in the shell: ⌘B from the sidebar
    /// closes the panel without dragging the keyboard into a session.
    private func closeShell(_ parent: SessionKey) {
        let hadFocus = activeShell(of: parent).flatMap { terminals[$0]?.state.isFocused } ?? false
        shellOpen.remove(parent)
        if shellFocusPending == parent { shellFocusPending = nil }
        applySurfaceVisibility()
        if hadFocus, parent == selectedKey { terminals[parent]?.focus() }
    }

    private func adoptNewShell() {
        guard let (parent, known) = awaitingShell,
              let fresh = shellKeys(for: parent).first(where: { !known.contains($0.id) })
        else { return }
        awaitingShell = nil
        activeShells[parent] = fresh
        applySurfaceVisibility()
    }

    /// Draws the selected session's visible shell if its panel is open, and hands it the
    /// keyboard if a shell command is still waiting on it. Called wherever a shell can
    /// newly exist or be shown: a `Sessions` frame, a selection change, a tab switch.
    ///
    /// Focus waits for `.running`: a stopped shell's terminal is about to be replaced by
    /// the relaunch, and focusing it would hand the keyboard to a surface on its way out.
    /// It also waits out a new tab, or the old one would take it first.
    private func revealShell() {
        guard let key = selectedKey, let shell = visibleShellKey, let terminal = terminals[shell]
        else { return }
        applySurfaceVisibility()
        if shellFocusPending == key, awaitingShell?.parent != key,
           info(for: shell)?.status == .running {
            shellFocusPending = nil
            terminal.focus()
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
        guard cols > 0, rows > 0 else { return }
        // A shell's surface already resized its own PTY; its grid is the panel's, not the
        // pane everyone else shares.
        if info(for: reporter)?.isShell == true {
            shellGrid = (cols, rows)
            Self.saveGrid(shellGrid, Self.shellGridKey)
            return
        }
        guard (cols, rows) != (pane.cols, pane.rows) else { return }
        pane = (cols, rows)
        Self.saveGrid(pane, Self.paneGridKey)
        let others = registry.resized(cols: cols, rows: rows).filter { effect in
            guard case let .send(vm, .resize(id, _, _)) = effect else { return true }
            return SessionKey(vm: vm, id: id) != reporter
        }
        apply(others)
    }

    // MARK: - commands

    func create(agent: String, name: String, cwd: String) {
        let vm = currentVM
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let display = name.isEmpty ? defaultName(cwd: cwd, agent: agent) : name
        send(vm, .create(agent: agent, name: display, cwd: cwd,
                         cols: pane.cols, rows: pane.rows))
        status = "starting \(agent) · \(display)…"
    }

    /// A dropped folder is a path on *this* machine, so it goes to the local VM whatever the
    /// sidebar selection is on — a remote would get a directory that does not exist there.
    /// An all-remote config has no local VM, and then the selection decides as before.
    func openFolders(_ paths: [String]) {
        guard let path = paths.last else { return }
        drop = (cwd: path, vm: vms.firstIndex { $0.config.isLocal })
        forgetDirCache()
        NSApp.activate()
        // A turn later, not now: a cold-launched open lands while the scene is still coming
        // up, and a sheet set before the window exists is dropped without a trace.
        Task { @MainActor in sheet = .newSession }
    }

    func clearDrop() { drop = nil }

    func kill(_ key: SessionKey) {
        send(key.vm, .kill(id: key.id))
        status = "killed \(key.id)"
    }

    func restartSelected() {
        guard let info = selectedInfo, info.status == .stopped, let key = selectedKey else { return }
        send(key.vm, .restart(id: key.id, cols: pane.cols, rows: pane.rows))
        status = "restarting \(info.name)…"
    }

    func killSelected() {
        guard let key = selectedKey, let info = selectedInfo else { return }
        sheet = .confirmKill(key, label: "\(info.agent) · \(info.name)")
    }

    /// The VM a new session would land on: the one a dropped folder lives on, else the one
    /// holding the selection, else the first.
    var currentVM: Int {
        if let vm = drop?.vm { return vm }
        if let selection, let row = allRows.first(where: { $0.id == selection }) {
            switch row {
            case let .folder(vm, _, _, _, _), let .elide(vm, _, _), let .session(vm, _, _):
                return vm
            }
        }
        return 0
    }

    /// cwd to seed a new session with: a dropped folder, the folder you are standing in, the
    /// one the selected session runs in, else home. Not the process cwd as in the TUI: an app
    /// launched by LaunchServices stands in `/`.
    var newCwd: String {
        if let cwd = drop?.cwd { return cwd }
        if let selection, let row = allRows.first(where: { $0.id == selection }) {
            switch row {
            case let .folder(_, path, _, _, _): return path
            case let .session(vm, id, _):
                if let s = vms[safe: vm]?.sessions.first(where: { $0.id == id }) { return s.cwd }
            case .elide: break
            }
        }
        return "~"
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
        send(currentVM, .listDir(path: dir))
    }

    /// Listings go stale as soon as anything could have changed on the far end.
    func forgetDirCache() {
        dirs.removeAll()
        requestedDirs.removeAll()
    }

    private static func dirKey(_ vm: Int, _ path: String) -> String { "\(vm)\u{0}\(path)" }

    // MARK: - folds and filter

    func isCollapsed(vm: Int, path: String) -> Bool {
        collapsed.contains(PathKey(vm: vm, path: path))
    }

    func toggleFold(vm: Int, path: String) {
        let key = PathKey(vm: vm, path: path)
        if collapsed.contains(key) { collapsed.remove(key) } else { collapsed.insert(key) }
        Self.save(collapsed, Self.foldsKey)
        rebuild()
    }

    func isFavourite(vm: Int, path: String) -> Bool {
        favourites.contains(PathKey(vm: vm, path: path))
    }

    /// A favourite folder stays in the sidebar with no sessions left in it, so it is still
    /// there to start the next one from.
    func toggleFavourite(vm: Int, path: String) {
        let key = PathKey(vm: vm, path: path)
        if favourites.contains(key) { favourites.remove(key) } else { favourites.insert(key) }
        Self.save(favourites, Self.favouritesKey)
        rebuild()
    }

    func toggleFavouriteAtSelection() {
        guard let selection,
              case let .folder(vm, path, _, _, _) = allRows.first(where: { $0.id == selection })
        else { return }
        toggleFavourite(vm: vm, path: path)
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
            // Companion shells ride along in the list but are reached with ⌘B, not a row.
            let idx = sessions.indices.filter { !sessions[$0].isShell && matches(sessions[$0]) }
            let folds = Set(collapsed.filter { $0.vm == vi }.map(\.path))
            // A filter is a search for sessions, so an empty favourite is noise in it.
            let favs = filter.isEmpty ? Set(favourites.filter { $0.vm == vi }.map(\.path)) : []
            return tree(sessions: sessions, idx: Array(idx),
                        collapsed: folds, favourites: favs).map { row in
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
        refreshWindowVisibility()
        let next = router.active(within: activityWindow)
        if next != activeDots { activeDots = next }
        let focused = [selectedKey, visibleShellKey].compactMap { $0 }
            .contains { terminals[$0]?.state.isFocused == true }
        if focused != terminalFocused { terminalFocused = focused }
        notifyBells()
    }

    /// An agent rings the bell when it wants you, and ghostty's parser is what tells a real
    /// BEL from the `ESC]0;…BEL` window title Claude Code sets on every turn.
    ///
    /// Every terminal is counted whether or not it will notify, so a bell you watched
    /// arrive is spent rather than waiting to fire the moment you look away.
    private func notifyBells() {
        let watching = NSApp.isActive ? selectedKey : nil
        for (key, terminal) in terminals {
            guard bells.rang(key, count: terminal.liveBells), key != watching else { continue }
            // A shell's bell is a failed tab completion, not an agent wanting you.
            guard let session = info(for: key), !session.isShell,
                  let vm = vms[safe: key.vm] else { continue }
            notifier.post(key,
                          title: "\(session.agent) · \(session.name)",
                          body: "\(vm.name) · \(session.cwd)")
        }
    }

    func info(for key: SessionKey) -> SessionInfo? {
        vms[safe: key.vm]?.sessions.first { $0.id == key.id }
    }

    func isOnline(_ vm: Int) -> Bool { vms[safe: vm]?.online ?? false }

    // MARK: - persistence

    // Folds were in-memory only in the Rust client and reset on every launch, which was
    // marked as debt there. Two lines here.
    private static let foldsKey = "collapsedFolders"
    private static let favouritesKey = "favouriteFolders"

    private static func load(_ key: String) -> Set<PathKey> {
        guard let data = UserDefaults.standard.data(forKey: key),
              let paths = try? JSONDecoder().decode(Set<PathKey>.self, from: data)
        else { return [] }
        return paths
    }

    private static func save(_ paths: Set<PathKey>, _ key: String) {
        guard let data = try? JSONEncoder().encode(paths) else { return }
        UserDefaults.standard.set(data, forKey: key)
    }

    // The last real grids, so the attach burst at launch leaves every PTY the size it
    // already is — rather than squeezing each to a placeholder and back before the first
    // surface lays out, which a shell answers by redrawing its prompt at the wrong width.
    private static let paneGridKey = "paneGrid"
    private static let shellGridKey = "shellGrid"

    private static func loadGrid(_ key: String) -> (cols: UInt16, rows: UInt16)? {
        guard let grid = UserDefaults.standard.array(forKey: key) as? [Int], grid.count == 2
        else { return nil }
        return (UInt16(clamping: grid[0]), UInt16(clamping: grid[1]))
    }

    private static func saveGrid(_ grid: (cols: UInt16, rows: UInt16), _ key: String) {
        UserDefaults.standard.set([Int(grid.cols), Int(grid.rows)], forKey: key)
    }
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
