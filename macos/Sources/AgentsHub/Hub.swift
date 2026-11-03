import AgentsHubCore
import AppKit
import Foundation

/// What every window shares: one link per VM, the attach set, what each daemon lists, the
/// spaces and folds, and the news worth a banner. A window is an `AppModel` — its own
/// selection, its own space and its own surfaces — so two of them are two clients of the
/// same sessions, the way two terminals running `tmux attach` are.
///
/// A PTY has one size, so one window at a time speaks for the sessions: the one used
/// last, as `WindowOrder` decides. Its grid is what every PTY is kept at and only its
/// surfaces' writes reach the daemon. Another window keeps drawing the same output at its
/// own size until you click into it, which resizes every session to it.
@MainActor
final class Hub: ObservableObject {
    @Published private(set) var vms: [VMState] = []
    @Published var status = ""
    @Published var error: DaemonError?
    /// Agents that finished a turn, or answered a prompt, while no window was looking.
    @Published private(set) var doneDots: Set<SessionKey> = []
    @Published private(set) var configError: String?
    /// Shared rather than per window: a session moved to a space is in it everywhere.
    /// Each window still chooses which space it shows.
    @Published var book: SpaceBook {
        didSet {
            guard book != oldValue else { return }
            Self.save(book, Self.spacesKey)
            eachWindow { $0.refreshRows() }
        }
    }
    @Published private(set) var collapsed: Set<PathKey>
    @Published private(set) var favourites: Set<PathKey>

    /// Completion listings, keyed by the VM that would host the session and the directory
    /// asked about. One request per directory serves every later keystroke.
    @Published private(set) var dirs: [String: [String]] = [:]

    private(set) var agentNames: [String] = []
    /// The owner's agent grid, which every agent PTY is kept at and new attaches start at.
    private(set) var pane = Hub.loadGrid(Hub.paneGridKey) ?? (80, 24)
    /// The owner's shell panel grid: only the size a brand-new shell starts at.
    private(set) var shellGrid = Hub.loadGrid(Hub.shellGridKey) ?? (80, 12)
    var pendingCreates = PendingCreates()

    private var registry = SessionRegistry()
    private var links: [VMLink] = []
    private let router = TerminalRouter()
    private var agents = AgentWatch()
    private let notifier = Notifier()
    private var requestedDirs: Set<String> = []
    private var dotTimer: Timer?
    private var windows: [ObjectIdentifier: WeakWindow] = [:]
    private var order = WindowOrder<ObjectIdentifier>()

    init() {
        book = Self.load(Self.spacesKey) ?? SpaceBook()
        collapsed = Self.load(Self.foldsKey) ?? []
        favourites = Self.load(Self.favouritesKey) ?? []
    }

    // MARK: - lifecycle

    /// Every window's scene runs this on appearing, so it has to be a no-op after the
    /// first rather than a second set of links.
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
        eachWindow { $0.refreshRows() }
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
        AppDelegate.onOpen = { [weak self] paths in self?.open(paths) }
        open(AppDelegate.pending)
        AppDelegate.pending = []

        // One timer for every dot, at a quarter of the Rust client's 16 ms frame rate:
        // SwiftUI redraws only what changed, so this only has to be fast enough to look
        // live. It replaces the whole dirty-flag frame loop.
        dotTimer = Timer.scheduledTimer(withTimeInterval: 0.25, repeats: true) { [weak self] _ in
            Task { @MainActor in self?.tick() }
        }
        // Nothing here is deadline work, so let the OS coalesce the wakeup with whatever
        // else it was going to run — four exact timer fires a second is pure idle energy.
        dotTimer?.tolerance = 0.1
    }

    /// A window with no surfaces yet. Only the daemon still has the history, so every
    /// window starts over from a fresh replay — feeding the new surfaces alone is not an
    /// option, since replay and live bytes share one stream per session per connection.
    /// Reconnect rather than re-attach: a daemon older than per-connection stream dedupe
    /// would send every byte twice. The first window at launch finds no links yet.
    func add(_ window: AppModel) {
        let id = ObjectIdentifier(window)
        windows[id] = WeakWindow(window)
        order.opened(id)
        window.refreshRows()
        links.forEach { $0.reconnect() }
        if !AppDelegate.pending.isEmpty, !links.isEmpty {
            open(AppDelegate.pending)
            AppDelegate.pending = []
        }
    }

    /// Closing a window frees its surfaces; the others carry on undisturbed. If it owned
    /// the sessions, the window used before it takes them over at its own size.
    func remove(_ window: AppModel) {
        let id = ObjectIdentifier(window)
        windows[id] = nil
        router.drop(id)
        if let next = order.closed(id).flatMap({ windows[$0]?.model }) {
            adoptGrids(of: next)
        }
    }

    /// The window you are using now: key, or typing. Every PTY follows its size.
    func activate(_ window: AppModel) {
        if order.used(ObjectIdentifier(window)) { adoptGrids(of: window) }
    }

    func owns(_ window: AppModel) -> Bool {
        order.owns(ObjectIdentifier(window))
    }

    /// Sent even when the size is the same: a kernel PTY only signals the guest when its
    /// size actually changes, so an unchanged session does not so much as repaint.
    private func adoptGrids(of window: AppModel) {
        pane = window.pane
        shellGrid = window.shellGrid
        Self.saveGrid(pane, Self.paneGridKey)
        Self.saveGrid(shellGrid, Self.shellGridKey)
        apply(registry.resized(cols: pane.cols, rows: pane.rows))
        for shell in window.mountedShells where registry.isAttached(shell) {
            send(shell.vm, .resize(id: shell.id, cols: shellGrid.cols, rows: shellGrid.rows))
        }
    }

    private func eachWindow(_ body: (AppModel) -> Void) {
        windows.values.compactMap(\.model).forEach(body)
    }

    // MARK: - link events

    /// `nonisolated` so the link queues can call it; hops to main itself.
    private nonisolated func handle(_ index: Int, _ event: LinkEvent) {
        // PTY bytes never touch the main actor: the router feeds ghostty's own serial
        // queue directly. Everything else is UI-rate and hops.
        if case let .frame(.output(id, data, _)) = event {
            router.deliver(SessionKey(vm: index, id: id), data)
            return
        }
        Task { @MainActor in self.onMain(index, event) }
    }

    private func onMain(_ index: Int, _ event: LinkEvent) {
        switch event {
        // `.up` only means ssh started. Online waits for the daemon's first frame, and
        // link trouble stays out of `status`, or every retry against an unreachable host
        // flashes "connected" and several of them take turns on the one line.
        case .up:
            apply(registry.connected(vm: index))
        case let .down(reason):
            vms[index].linkError = reason ?? "offline"
            apply(registry.disconnected(vm: index))
            guard vms[index].online else { return }
            vms[index].online = false
            eachWindow { $0.refreshRows() }
        case let .frame(frame):
            if !vms[index].online {
                vms[index].online = true
                vms[index].linkError = nil
            }
            onFrame(index, frame)
        }
    }

    private func onFrame(_ index: Int, _ frame: Resp) {
        switch frame {
        case let .sessions(list):
            let known = Set(vms[index].sessions.map(\.id))
            vms[index].sessions = list
            claimCreated(index, list.filter { !known.contains($0.id) })
            book.prune(vm: vms[index].name, alive: Set(list.map(\.id)))
            apply(registry.sessions(vm: index, list, cols: pane.cols, rows: pane.rows,
                                    shellCols: shellGrid.cols, shellRows: shellGrid.rows))
            readActivity(index, list)
            eachWindow { $0.sessionsChanged() }
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
            status = "\(vms[index].name): \(msg.prefix { $0 != "\n" })"
            error = DaemonError(vm: vms[index].name, message: msg)
            eachWindow { $0.daemonError(vm: index) }
        case .output:
            break  // handled off-main in `handle`
        }
    }

    // MARK: - effects

    func send(_ vm: Int, _ req: Req) {
        links[safe: vm]?.send(req)
    }

    private func apply(_ effects: [Effect]) {
        for effect in effects {
            switch effect {
            case let .send(vm, req):
                send(vm, req)
            case let .teardown(key):
                eachWindow { $0.makeTerminal(key) }
            case let .forget(key):
                router.forget(key)
                agents.forget(key)
                eachWindow { $0.forget(key) }
            }
        }
    }

    // MARK: - terminals

    func route(_ key: SessionKey, for window: AppModel, _ feed: AttachFeed) {
        router.set(key, for: ObjectIdentifier(window), feed)
    }

    /// Keystrokes, and ghostty's answers to the guest's queries, which every window's
    /// surface gives: only the owner's reach the PTY, or each query would be answered once
    /// per window.
    func terminalSent(_ req: Req, vm: Int, from window: AppModel) {
        guard owns(window) else { return }
        send(vm, req)
    }

    /// A surface laid out a new grid. The reporter's own PTY follows only if its window
    /// owns the sessions; otherwise that window just draws at its own size until used.
    ///
    /// One geometry for every agent pane, as in the Rust client, but taken from whichever
    /// surface actually laid out a grid rather than derived from view size — the cell
    /// size follows the user's ghostty font and is not ours to guess. Everyone else's
    /// surface may not be there to notice: not laid out yet, or in no window at all.
    func gridChanged(from window: AppModel, _ reporter: SessionKey,
                     cols: UInt16, rows: UInt16, isShell: Bool) {
        guard owns(window) else { return }
        send(reporter.vm, .resize(id: reporter.id, cols: cols, rows: rows))
        // A shell's grid is the panel's, not the pane everyone else shares.
        if isShell {
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

    func kill(_ key: SessionKey) {
        send(key.vm, .kill(id: key.id))
        status = "killed \(key.id)"
    }

    /// The tab's ×, and a shell's own `exit`. Every window showing that tab moves to its
    /// neighbour.
    func closeShellTab(_ shell: SessionKey) {
        guard info(for: shell)?.parent != nil else { return }
        send(shell.vm, .kill(id: shell.id))
        eachWindow { $0.shellTabClosed(shell) }
    }

    /// A Dock drop or `agents-hub open` goes to the window you used last. With none open
    /// it waits for the one the reopen brings up.
    private func open(_ paths: [String]) {
        guard !paths.isEmpty else { return }
        let owner = order.owner.flatMap { windows[$0]?.model }
        if let window = owner ?? windows.values.lazy.compactMap(\.model).first {
            window.openFolders(paths)
        } else {
            AppDelegate.pending += paths
            AppDelegate.reopen?()
        }
    }

    // MARK: - cwd completion

    func listing(vm: Int, _ dir: String) -> [String] {
        dirs[Self.dirKey(vm, dir)] ?? []
    }

    /// Asks the VM that would host the session what lives in the directory being typed.
    /// Only the first ask per directory goes out; the reply serves every later keystroke.
    func requestDirs(vm: Int, _ dir: String) {
        let key = Self.dirKey(vm, dir)
        guard !requestedDirs.contains(key) else { return }
        requestedDirs.insert(key)
        send(vm, .listDir(path: dir))
    }

    /// Listings go stale as soon as anything could have changed on the far end.
    func forgetDirCache() {
        dirs.removeAll()
        requestedDirs.removeAll()
    }

    private static func dirKey(_ vm: Int, _ path: String) -> String { "\(vm)\u{0}\(path)" }

    // MARK: - folds

    func toggleFold(_ key: PathKey) {
        if collapsed.contains(key) { collapsed.remove(key) } else { collapsed.insert(key) }
        Self.save(collapsed, Self.foldsKey)
        eachWindow { $0.refreshRows() }
    }

    func toggleFavourite(_ key: PathKey) {
        if favourites.contains(key) { favourites.remove(key) } else { favourites.insert(key) }
        Self.save(favourites, Self.favouritesKey)
        eachWindow { $0.refreshRows() }
    }

    // MARK: - spaces

    func member(_ key: SessionKey) -> MemberKey? {
        vms[safe: key.vm].map { MemberKey(vm: $0.name, id: key.id) }
    }

    func spaceName(of key: SessionKey) -> String? {
        member(key).flatMap(book.space(of:)).flatMap(book.space)?.name
    }

    private func claimCreated(_ vm: Int, _ fresh: [SessionInfo]) {
        for (id, space) in pendingCreates.claim(vm: vm, fresh) {
            if let key = member(SessionKey(vm: vm, id: id)) { book.move([key], to: space) }
        }
    }

    // MARK: - activity

    /// ponytail: focus and "seen" ride the dot timer rather than a Combine subscription
    /// per terminal, so either can lag a quarter second. Subscribe to `state.$isFocused`
    /// if that ever shows.
    private func tick() {
        eachWindow { $0.tick() }
        let watching = watchedKeys
        guard !watching.isEmpty else { return }
        watching.forEach { agents.seen($0) }
        publishDone()
    }

    /// The sessions actually in front of you, one per window at most. Their news is never
    /// news.
    private var watchedKeys: Set<SessionKey> {
        Set(windows.values.compactMap { $0.model?.watchingKey })
    }

    /// The daemon reads each agent's screen and says what it is doing; a prompt waiting
    /// on you, or a turn that ended while you were elsewhere, is worth a banner.
    private func readActivity(_ vm: Int, _ list: [SessionInfo]) {
        let watching = watchedKeys
        for session in list where !session.isShell {
            let key = SessionKey(vm: vm, id: session.id)
            guard let news = agents.update(key, session.activity, watching: watching.contains(key)),
                  let host = vms[safe: vm] else { continue }
            let what = news == .needsYou ? "needs you" : "finished"
            let space = spaceName(of: key).map { "\($0) · " } ?? ""
            notifier.post(key,
                          title: "\(space)\(session.agent) · \(session.name) \(what)",
                          body: "\(host.name) · \(session.cwd)")
        }
        publishDone()
    }

    private func publishDone() {
        if agents.done != doneDots { doneDots = agents.done }
    }

    func info(for key: SessionKey) -> SessionInfo? {
        vms[safe: key.vm]?.sessions.first { $0.id == key.id }
    }

    // MARK: - persistence

    // Folds were in-memory only in the Rust client and reset on every launch, which was
    // marked as debt there. Two lines here.
    private static let foldsKey = "collapsedFolders"
    private static let favouritesKey = "favouriteFolders"
    private static let spacesKey = "spaces"

    static func load<T: Decodable>(_ key: String) -> T? {
        guard let data = UserDefaults.standard.data(forKey: key) else { return nil }
        return try? JSONDecoder().decode(T.self, from: data)
    }

    static func save<T: Encodable>(_ value: T, _ key: String) {
        guard let data = try? JSONEncoder().encode(value) else { return }
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

private struct WeakWindow {
    weak var model: AppModel?

    init(_ model: AppModel) { self.model = model }
}
