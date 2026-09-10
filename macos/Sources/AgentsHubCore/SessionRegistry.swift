import Foundation

public struct SessionKey: Hashable, Sendable, Comparable {
    public let vm: Int
    public let id: String

    public init(vm: Int, id: String) {
        self.vm = vm
        self.id = id
    }

    public static func < (a: SessionKey, b: SessionKey) -> Bool {
        (a.vm, a.id) < (b.vm, b.id)
    }
}

/// What the registry decided; the app applies these and does nothing else to the attach
/// set. Keeping the rules here — rather than scattered across frame handlers the way the
/// Rust client had to — is what makes them testable without a GPU or a daemon.
public enum Effect: Equatable, Sendable {
    case send(vm: Int, req: Req)
    /// Drop any terminal for this key and build a fresh one. Idempotent: a brand-new
    /// session and a restarted one take the same path.
    case teardown(SessionKey)
    /// The session is gone from the daemon entirely; forget it.
    case forget(SessionKey)
}

/// Who is attached to what, and the three rules that were each a bug in the Rust client.
public struct SessionRegistry {
    private(set) public var attached: Set<SessionKey> = []
    private var known: [Int: [String: Status]] = [:]

    public init() {}

    public func isAttached(_ key: SessionKey) -> Bool { attached.contains(key) }

    /// Rule 1: a reconnected daemon has no memory of the dropped connection's
    /// subscriptions, so every attachment for this VM is void. Re-`List` afterwards;
    /// the `Sessions` frame that comes back drives the re-attach.
    public mutating func connected(vm: Int) -> [Effect] {
        let mine = attached.filter { $0.vm == vm }.sorted()
        attached.subtract(mine)
        known[vm] = nil
        return mine.map { .teardown($0) } + [.send(vm: vm, req: .list)]
    }

    /// The link dropped. Same voiding, but nothing to send — `connected` re-lists.
    public mutating func disconnected(vm: Int) -> [Effect] {
        let mine = attached.filter { $0.vm == vm }.sorted()
        attached.subtract(mine)
        known[vm] = nil
        return mine.map { .teardown($0) }
    }

    /// Rule 2: `Sessions` frames arrive unsolicited on every daemon-side change, so this
    /// must be idempotent and must never send `List`.
    ///
    /// Rule 3: `Req.restart` reuses the session id, but the old broadcast channel died
    /// with the old PTY — a surviving attachment would go silent forever. Any id that
    /// went Stopped → Running is therefore dropped from `attached` first, so it takes
    /// exactly the same path a dropped connection takes.
    public mutating func sessions(
        vm: Int, _ list: [SessionInfo], cols: UInt16, rows: UInt16
    ) -> [Effect] {
        let previous = known[vm] ?? [:]
        var live: [String: Status] = [:]
        for s in list { live[s.id] = s.status }
        known[vm] = live

        var effects: [Effect] = []

        for id in Set(previous.keys).subtracting(live.keys).sorted() {
            let key = SessionKey(vm: vm, id: id)
            attached.remove(key)
            effects.append(.forget(key))
        }
        // Also forget anything we were attached to that this frame doesn't list — the
        // first frame after a restart has no `previous` to diff against.
        for key in attached.filter({ $0.vm == vm && live[$0.id] == nil }).sorted() {
            attached.remove(key)
            effects.append(.forget(key))
        }

        for id in live.keys.sorted() {
            let key = SessionKey(vm: vm, id: id)
            if previous[id] == .stopped, live[id] == .running {
                attached.remove(key)
            }
            guard !attached.contains(key) else { continue }
            attached.insert(key)
            effects.append(.teardown(key))
            // Attach doubles as a resize server-side, so no separate Resize is needed.
            effects.append(.send(vm: vm, req: .attach(id: id, cols: cols, rows: rows)))
        }
        return effects
    }

    /// One geometry for every pane, as in the Rust client. Unmounted sessions are
    /// included deliberately: they have no surface to report their own size, and without
    /// this their PTY sits at 80×24 while the window is 200 columns wide.
    public func resized(cols: UInt16, rows: UInt16) -> [Effect] {
        attached.sorted().map { .send(vm: $0.vm, req: .resize(id: $0.id, cols: cols, rows: rows)) }
    }
}
