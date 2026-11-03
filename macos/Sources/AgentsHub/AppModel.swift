import AgentsHubCore
import Combine
import Foundation
import SwiftUI

struct VMState {
    let config: VMConfig
    var online = false
    /// Why the last attempt failed, kept until the daemon answers again.
    var linkError: String?
    var sessions: [SessionInfo] = []

    var name: String { config.name }
}

/// A sidebar row. The id must be stable and content-derived: the list is rebuilt on every
/// `Sessions` frame, and array indices would make the selection jump under the user.
enum SidebarRow: Identifiable, Hashable {
    case vm(Int)
    case folder(vm: Int, path: String, seg: String, depth: Int, hasSub: Bool)
    case elide(vm: Int, path: String, depth: Int)
    case session(vm: Int, id: String, depth: Int)

    var id: String {
        switch self {
        case let .vm(vm): return "v/\(vm)"
        case let .folder(vm, path, _, _, _): return "f/\(vm)/\(path)"
        case let .elide(vm, path, _): return "e/\(vm)/\(path)"
        case let .session(vm, id, _): return "s/\(vm)/\(id)"
        }
    }

    var depth: Int {
        switch self {
        case .vm: return 0
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
    case newSpace
    case renameSpace(UUID)
    case deleteSpace(UUID)
    case moveToSpace([SessionKey], label: String)
    case goTo

    var id: String {
        switch self {
        case .newSession: return "new"
        case let .confirmKill(k, _): return "kill/\(k.vm)/\(k.id)"
        case .newSpace: return "space/new"
        case let .renameSpace(id): return "space/rename/\(id)"
        case let .deleteSpace(id): return "space/delete/\(id)"
        case let .moveToSpace(keys, _): return "space/move/" + keys.map { "\($0.vm)/\($0.id)" }.joined(separator: ",")
        case .goTo: return "goto"
        }
    }
}

/// A daemon's `Error` in full. The status bar has one line, and the cause — "No viable
/// candidates found in PATH" — is usually on the next.
struct DaemonError: Identifiable {
    let id = UUID()
    let vm: String
    let message: String
}

/// One window: its selection, the space it shows, its ⌘B panel, and a surface of its own
/// for every session. The twin of `src/tui/app.rs` and the `on_msg` half of `event.rs`,
/// with everything windows share — links, the attach set, spaces — in `Hub`.
///
/// Every mutation of the attach set goes through the hub's `SessionRegistry`, which
/// returns effects the hub applies, building terminals here. Nothing else may touch it —
/// that is what keeps the three rules that each cost the Rust client a bug in one
/// testable place.
@MainActor
final class AppModel: ObservableObject {
    let hub: Hub

    @Published private(set) var rowsByVM: [[SidebarRow]] = []
    @Published var selection: SidebarRow.ID?
    @Published var filter = "" { didSet { rebuild() } }
    @Published var sheet: Sheet?
    /// Toggled by ⌘L; the root view watches it and moves focus. A plain signal rather
    /// than reaching into `@FocusState` from the model.
    @Published var requestSidebarFocus = false
    /// Every agent terminal's surface, never unmounted: the surface is the scrollback.
    /// Append-only rather than derived from `terminals`, so a new session never reorders
    /// the views under the focused one — a moved view resigns first responder.
    @Published private(set) var mounted: [SessionKey] = []
    /// Agent sessions whose ⌘B shell panel is open. Per session, so switching to one
    /// without a shell does not open an empty panel under it.
    @Published private(set) var shellOpen: Set<SessionKey> = []
    /// `mounted`'s twin for the companion shells in the panel.
    @Published private(set) var mountedShells: [SessionKey] = []
    /// The panel tab each agent session last showed. A missing or dead entry falls back
    /// to its first shell.
    @Published private(set) var activeShells: [SessionKey: SessionKey] = [:]
    /// Who owns the keyboard, as ghostty sees it — SwiftUI's own `@FocusState` doesn't
    /// notice a click landing straight in the surface, and an indicator that lies is
    /// worse than none.
    @Published private(set) var terminalFocused = false
    @Published private(set) var showing = SpaceView.all {
        didSet { if showing != oldValue { Hub.save(showing, Self.viewKey) } }
    }

    private(set) var terminals: [SessionKey: TerminalSession] = [:]
    /// This window's agent grid, as its surfaces laid it out. Every PTY takes it while
    /// this window owns the sessions.
    private(set) var pane: (cols: UInt16, rows: UInt16)
    /// This window's shell panel grid, reported by whichever shell surface laid out last.
    private(set) var shellGrid: (cols: UInt16, rows: UInt16)

    /// Where each view's selection was when you left it, so switching back lands there.
    private var lastSelection: [SpaceView: SidebarRow.ID] = [:]
    /// The VM picked in the New Session sheet, which a space spanning machines needs and an
    /// empty space has no row to imply.
    @Published private var pickedVM: Int?
    /// The session whose shell ⌘B just asked for, so it takes the keyboard the moment it
    /// has a live surface — which, for a first open, is a round trip to the daemon later.
    private var shellFocusPending: SessionKey?
    /// A new tab asked of the daemon, and the shells its parent had before — the one not
    /// among them when it shows up is the tab to switch to.
    private var awaitingShell: (parent: SessionKey, known: Set<String>)?
    private var windowVisible = true
    private weak var paneWindow: NSWindow?
    private var hubChanges: AnyCancellable?

    /// Set only while the New Session sheet is showing a dropped folder, and cleared when it
    /// closes. It overrides the VM the selection implies, so cwd completion and `create` both
    /// target the machine the path is actually on — every reader of `currentVM` is
    /// sheet-scoped, which is what makes overriding it safe.
    private var drop: (cwd: String, vm: Int?)?

    /// A new window opens on the space the last one chose, at the grid the sessions
    /// already have.
    init(hub: Hub) {
        self.hub = hub
        pane = hub.pane
        shellGrid = hub.shellGrid
        showing = Hub.load(Self.viewKey).flatMap { hub.book.has($0) ? $0 : nil } ?? .all
        // Views read the shared state through this model, so they redraw when it moves.
        hubChanges = hub.objectWillChange.sink { [weak self] _ in self?.objectWillChange.send() }
    }

    // MARK: - shared state, as this window sees it

    var vms: [VMState] { hub.vms }
    var status: String {
        get { hub.status }
        set { hub.status = newValue }
    }
    var error: DaemonError? {
        get { hub.error }
        set { hub.error = newValue }
    }
    var doneDots: Set<SessionKey> { hub.doneDots }
    var configError: String? { hub.configError }
    var agentNames: [String] { hub.agentNames }
    var book: SpaceBook { hub.book }

    // MARK: - lifecycle

    /// Joins the hub, which reconnects so this window's surfaces get a replay of their own.
    func windowAppeared() {
        hub.add(self)
    }

    /// Closing the window frees every surface, so this window's terminals go with it. If
    /// the same window comes back it joins again and is rebuilt from a fresh replay.
    func windowClosed() {
        hub.remove(self)
        terminals.removeAll()
        mounted.removeAll()
        mountedShells.removeAll()
        terminalFocused = false
    }

    /// Became key: every PTY follows this window's size from now on.
    func windowBecameKey() {
        hub.activate(self)
    }

    /// Rows and selection after anything that changes what the sidebar can show. A space
    /// another window deleted takes this one to wherever deleting it would have.
    func refreshRows() {
        guard hub.book.has(showing) else {
            return selectSpace(hub.book.landing(unsorted: sessions(in: .unsorted).count))
        }
        let previous = allRows
        rebuild()
        pruneSelection(near: previous)
    }

    /// A `Sessions` frame landed and the hub has applied it.
    func sessionsChanged() {
        refreshRows()
        adoptNewShell()
        revealShell()
    }

    /// Most likely a daemon that predates `Shell` and has no idea what we asked for.
    /// Leaving the panel open would be a blank promise that never resolves.
    func daemonError(vm: Int) {
        if let parent = shellFocusPending, parent.vm == vm, shellKeys(for: parent).isEmpty {
            closeShell(parent)
        }
    }

    // MARK: - terminals

    /// Mounted at once, selected or not: an unmounted terminal's bytes pile up in the
    /// feed's backlog, and a busy agent overflows it — trimming the mode prelude, so the
    /// screen is garbled and paste arrives unbracketed.
    ///
    /// First responder does not follow a surface to its replacement, so it is handed on.
    func makeTerminal(_ key: SessionKey) {
        let vm = key.vm
        let hadFocus = terminals[key]?.ownsKeyboard ?? false
        let terminal = TerminalSession(
            key: key,
            send: { [weak self] req in
                // Off the main actor: this is ghostty's write callback, at keystroke rate.
                Task { @MainActor in self?.terminalSent(req, vm: vm) }
            },
            onGrid: { [weak self] cols, rows in
                Task { @MainActor in self?.gridChanged(key, cols: cols, rows: rows) }
            }
        )
        terminals[key] = terminal
        hub.route(key, for: self, terminal.feed)
        if info(for: key)?.isShell == true {
            if !mountedShells.contains(key) { mountedShells.append(key) }
        } else if !mounted.contains(key) {
            mounted.append(key)
        }
        applySurfaceVisibility()
        if hadFocus { terminal.focus() }
    }

    func forget(_ key: SessionKey) {
        terminals[key] = nil
        mounted.removeAll { $0 == key }
        mountedShells.removeAll { $0 == key }
        shellOpen.remove(key)
        activeShells[key] = nil
        if shellFocusPending == key { shellFocusPending = nil }
        if awaitingShell?.parent == key { awaitingShell = nil }
    }

    /// Typing goes to the window it was typed in, and makes that window the one in use —
    /// a safety net for a key-window notice that never came.
    private func terminalSent(_ req: Req, vm: Int) {
        if case .input = req, paneWindow?.isKeyWindow == true { hub.activate(self) }
        hub.terminalSent(req, vm: vm, from: self)
    }

    private func gridChanged(_ reporter: SessionKey, cols: UInt16, rows: UInt16) {
        guard cols > 0, rows > 0 else { return }
        let isShell = info(for: reporter)?.isShell == true
        if isShell { shellGrid = (cols, rows) } else { pane = (cols, rows) }
        hub.gridChanged(from: self, reporter, cols: cols, rows: rows, isShell: isShell)
    }

    // MARK: - selection

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

    var selectionIsVM: Bool {
        guard let selection, case .vm = allRows.first(where: { $0.id == selection })
        else { return false }
        return true
    }

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
    /// the window is on screen. Hidden surfaces keep parsing, so their scrollback is
    /// current the moment you switch to them.
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
        hub.send(key.vm, .shell(parent: key.id, cols: shellGrid.cols, rows: shellGrid.rows, new: true))
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

    /// A stopped tab is relaunched on sight — after a daemon restart every one is — by the
    /// window in use, so two windows moving to the same tab do not relaunch it twice.
    private func show(_ shell: SessionKey, of parent: SessionKey, focus: Bool) {
        activeShells[parent] = shell
        shellOpen.insert(parent)
        if focus { shellFocusPending = parent }
        if info(for: shell)?.status == .stopped, hub.owns(self) {
            hub.send(shell.vm, .restart(id: shell.id, cols: shellGrid.cols, rows: shellGrid.rows))
        }
        applySurfaceVisibility()
        revealShell()
    }

    /// The tab's ×. The shell is gone for every window, so the hub tells each one.
    func closeShellTab(_ shell: SessionKey) {
        hub.closeShellTab(shell)
    }

    /// The panel closes with its last tab, and the keyboard follows the tab that takes the
    /// closed one's place only if it was there.
    func shellTabClosed(_ shell: SessionKey) {
        guard let parentID = info(for: shell)?.parent else { return }
        let parent = SessionKey(vm: shell.vm, id: parentID)
        let shells = shellKeys(for: parent)
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
    ///
    /// Also seeds the first selection: an app that opens onto an empty pane when there
    /// are sessions right there makes you click before it does anything.
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

    // MARK: - commands

    func create(agent: String, name: String, cwd: String) {
        let vm = currentVM
        let name = name.trimmingCharacters(in: .whitespacesAndNewlines)
        let display = name.isEmpty ? defaultName(cwd: cwd, agent: agent) : name
        if case let .space(space) = showing {
            hub.pendingCreates.expect(vm: vm, agent: agent, name: display, space: space)
        }
        hub.send(vm, .create(agent: agent, name: display, cwd: cwd,
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
        paneWindow?.makeKeyAndOrderFront(nil)
        // A turn later, not now: a cold-launched open lands while the scene is still coming
        // up, and a sheet set before the window exists is dropped without a trace.
        Task { @MainActor in sheet = .newSession }
    }

    func clearDrop() {
        drop = nil
        pickedVM = nil
    }

    /// The sheet's VM picker. A dropped folder still wins: it only exists on this machine.
    func pickVM(_ vm: Int) {
        pickedVM = vm
        forgetDirCache()
    }

    func kill(_ key: SessionKey) {
        hub.kill(key)
    }

    func restartSelected() {
        guard let info = selectedInfo, info.status == .stopped, let key = selectedKey else { return }
        hub.send(key.vm, .restart(id: key.id, cols: pane.cols, rows: pane.rows))
        status = "restarting \(info.name)…"
    }

    func killSelected() {
        guard let key = selectedKey, let info = selectedInfo else { return }
        sheet = .confirmKill(key, label: "\(info.agent) · \(info.name)")
    }

    /// The VM a new session would land on: the one a dropped folder lives on, else the one
    /// picked in the sheet, else the one holding the selection, else the first.
    var currentVM: Int {
        if let vm = drop?.vm { return vm }
        if let vm = pickedVM { return vm }
        if let selection, let row = allRows.first(where: { $0.id == selection }) {
            switch row {
            case let .vm(vm), let .folder(vm, _, _, _, _), let .elide(vm, _, _),
                 let .session(vm, _, _):
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
            case .vm, .elide: break
            }
        }
        return "~"
    }

    // MARK: - cwd completion

    func cwdMatches(_ cwd: String) -> [String] {
        guard let (dir, typed) = splitCwd(cwd) else { return [] }
        return filterDirs(hub.listing(vm: currentVM, dir), typed)
    }

    func requestDirs(_ cwd: String) {
        guard let (dir, _) = splitCwd(cwd) else { return }
        hub.requestDirs(vm: currentVM, dir)
    }

    func forgetDirCache() {
        hub.forgetDirCache()
    }

    // MARK: - spaces

    private func member(_ key: SessionKey) -> MemberKey? {
        hub.member(key)
    }

    private func shows(_ key: SessionKey, in view: SpaceView? = nil) -> Bool {
        member(key).map { book.contains(view ?? showing, $0) } ?? false
    }

    /// Every agent session the view holds; shells ride with their parent.
    func sessions(in view: SpaceView) -> [SessionKey] {
        vms.indices.flatMap { vi in
            vms[vi].sessions.filter { !$0.isShell }.map { SessionKey(vm: vi, id: $0.id) }
        }
        .filter { shows($0, in: view) }
    }

    func mood(of key: SessionKey) -> Mood {
        guard let info = info(for: key) else { return .empty }
        return Mood(online: isOnline(key.vm), status: info.status, activity: info.activity,
                    done: doneDots.contains(key))
    }

    func mood(of view: SpaceView) -> Mood {
        Mood.rollup(sessions(in: view).map(mood(of:)))
    }

    var barViews: [SpaceView] {
        book.bar(showing: showing, unsorted: sessions(in: .unsorted).count)
    }

    func title(of view: SpaceView) -> String {
        switch view {
        case let .space(id): return book.space(id)?.name ?? "?"
        case .unsorted: return "Unsorted"
        case .all: return "All"
        }
    }

    var activeSpace: Space? {
        if case let .space(id) = showing { return book.space(id) }
        return nil
    }

    func spaceID(of key: SessionKey) -> UUID? {
        member(key).flatMap(book.space(of:))
    }

    func spaceName(of key: SessionKey) -> String? {
        hub.spaceName(of: key)
    }

    func selectSpace(_ next: SpaceView) {
        guard book.has(next), next != showing else { return }
        lastSelection[showing] = selection
        showing = next
        rebuild()
        let remembered = lastSelection[next].flatMap { id in
            allRows.contains { $0.id == id } ? id : nil
        }
        selection = remembered ?? allRows.first { $0.key != nil }?.id ?? allRows.first?.id
        selectionChanged()
    }

    func stepSpace(by delta: Int) {
        selectSpace(showing.stepped(by: delta, in: barViews))
    }

    func createSpace(named name: String) {
        guard let name = Self.cleanName(name) else { return }
        selectSpace(.space(hub.book.create(name: name)))
    }

    func renameSpace(_ id: UUID, to name: String) {
        guard let name = Self.cleanName(name) else { return }
        hub.book.rename(id, to: name)
    }

    /// An empty space goes at once; one holding sessions asks first. Either way its
    /// sessions only move to Unsorted, and every window showing it moves on.
    func deleteSpace(_ id: UUID, confirmed: Bool = false) {
        guard book.space(id) != nil else { return }
        if !confirmed, !sessions(in: .space(id)).isEmpty {
            sheet = .deleteSpace(id)
            return
        }
        hub.book.delete(id)
    }

    /// What `m` moves: the selected session, or everything this view shows under the
    /// selected folder.
    private var movableSelection: [SessionKey] {
        guard let selection, let row = allRows.first(where: { $0.id == selection }) else { return [] }
        switch row {
        case let .session(vm, id, _): return [SessionKey(vm: vm, id: id)]
        case let .folder(vm, path, _, _, _): return sessionKeys(under: path, vm: vm)
        case .vm, .elide: return []
        }
    }

    private func sessionKeys(under folder: String, vm: Int) -> [SessionKey] {
        (vms[safe: vm]?.sessions ?? [])
            .filter { !$0.isShell && matches($0) && cwd($0.cwd, isUnder: folder) }
            .map { SessionKey(vm: vm, id: $0.id) }
            .filter { shows($0) }
    }

    func moveSelection() {
        let keys = movableSelection
        guard let first = keys.first else { return }
        let label = keys.count > 1
            ? "\(keys.count) sessions"
            : info(for: first).map { "\($0.agent) · \($0.name)" } ?? first.id
        sheet = .moveToSpace(keys, label: label)
    }

    /// `nil` is Unsorted. Sessions that no longer belong in a window's view leave it, and
    /// its selection goes to a neighbour as it does after a kill.
    func move(_ keys: [SessionKey], to space: UUID?) {
        hub.book.move(keys.compactMap(member), to: space)
        let target = title(of: space.map(SpaceView.space) ?? .unsorted)
        status = "\(keys.count) session\(keys.count == 1 ? "" : "s") moved to \(target)"
    }

    func move(_ keys: [SessionKey], toNewSpace name: String) {
        guard let name = Self.cleanName(name) else { return }
        move(keys, to: hub.book.create(name: name))
    }

    /// A drop on a chip, carrying sidebar row ids, of which only sessions count.
    func move(rowIDs: [String], to target: SpaceView) {
        let keys = rowIDs.compactMap { id in allRows.first { $0.id == id }?.key }
        switch target {
        case let .space(id): move(keys, to: id)
        case .unsorted: move(keys, to: nil)
        case .all: break
        }
    }

    /// `s` on a folder: a space named after it, holding what this view shows under it. A
    /// space of that name already there takes them rather than gaining a twin.
    func newSpaceFromSelectedFolder() {
        guard let selection,
              case let .folder(vm, path, seg, _, _) = allRows.first(where: { $0.id == selection })
        else { return }
        let keys = sessionKeys(under: path, vm: vm)
        let id = hub.book.space(named: seg)
        hub.book.move(keys.compactMap(member), to: id)
        status = "\(keys.count) session\(keys.count == 1 ? "" : "s") in \(seg)"
        selectSpace(.space(id))
    }

    /// ⌘P on a session: its space comes to the front, and a filter that would hide it
    /// goes, so the row it lands on is actually in the list.
    func reveal(_ key: SessionKey) {
        if !shows(key) {
            let home = spaceID(of: key).map(SpaceView.space)
            selectSpace(home ?? (book.spaces.isEmpty ? .all : .unsorted))
        }
        let id = SidebarRow.session(vm: key.vm, id: key.id, depth: 0).id
        if !allRows.contains(where: { $0.id == id }) { filter = "" }
        selection = id
        selectionChanged()
    }

    private static func cleanName(_ raw: String) -> String? {
        let name = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        return name.isEmpty ? nil : name
    }

    // MARK: - folds and filter

    func isCollapsed(vm: Int, path: String) -> Bool {
        hub.collapsed.contains(PathKey(vm: vm, path: path))
    }

    func toggleFold(vm: Int, path: String) {
        hub.toggleFold(PathKey(vm: vm, path: path))
    }

    func isFavourite(vm: Int, path: String) -> Bool {
        hub.favourites.contains(PathKey(vm: vm, path: path))
    }

    /// A favourite folder stays in the sidebar with no sessions left in it, so it is still
    /// there to start the next one from.
    func toggleFavourite(vm: Int, path: String) {
        hub.toggleFavourite(PathKey(vm: vm, path: path))
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
            let idx = sessions.indices.filter {
                !sessions[$0].isShell && matches(sessions[$0])
                    && shows(SessionKey(vm: vi, id: sessions[$0].id))
            }
            let folds = Set(hub.collapsed.filter { $0.vm == vi }.map(\.path))
            // A filter is a search for sessions, so an empty favourite is noise in it. So
            // is it inside a space: favourites are per machine, and every space would grow
            // every one of them.
            let favs = filter.isEmpty && showing == .all
                ? Set(hub.favourites.filter { $0.vm == vi }.map(\.path)) : []
            let rows: [SidebarRow] = tree(sessions: sessions, idx: Array(idx),
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
            // The VM's own row is what makes an empty VM selectable, and so a target for ⌘N.
            // A space lists only the machines it has sessions on; the sheet picks the rest.
            if showing != .all, rows.isEmpty { return [] }
            return [.vm(vi)] + rows
        }
    }

    // MARK: - activity

    /// The hub's dot timer, a quarter second apart.
    func tick() {
        refreshWindowVisibility()
        let focused = [selectedKey, visibleShellKey].compactMap { $0 }
            .contains { terminals[$0]?.state.isFocused == true }
        if focused != terminalFocused { terminalFocused = focused }
    }

    /// The session actually in front of you: selected, in a window on screen, in the
    /// app you are using.
    var watchingKey: SessionKey? {
        NSApp.isActive && windowVisible ? selectedKey : nil
    }

    func info(for key: SessionKey) -> SessionInfo? {
        hub.info(for: key)
    }

    func isOnline(_ vm: Int) -> Bool { vms[safe: vm]?.online ?? false }

    /// Every unreachable VM and why, in config order: one steady line however many fail.
    var linkErrors: String {
        vms.filter { !$0.online }
            .compactMap { vm in vm.linkError.map { "\(vm.name): \($0)" } }
            .joined(separator: " · ")
    }

    private static let viewKey = "activeSpace"
}

extension Array {
    subscript(safe index: Int) -> Element? {
        indices.contains(index) ? self[index] : nil
    }
}
