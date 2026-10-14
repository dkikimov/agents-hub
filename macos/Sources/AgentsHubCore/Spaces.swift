import Foundation

/// Spaces: named sets of sessions the user put together, one per project. Membership is
/// the client's own bookkeeping — the daemon never hears of it, the same way it never
/// learns its VM name — so a space can hold sessions from several VMs.

/// A session as a space remembers it: by VM *name*, so reordering `[[vm]]` in the config
/// does not hand a space another machine's sessions.
public struct MemberKey: Hashable, Codable, Sendable {
    public let vm: String
    public let id: String

    public init(vm: String, id: String) {
        self.vm = vm
        self.id = id
    }
}

public struct Space: Identifiable, Codable, Equatable, Sendable {
    public let id: UUID
    public var name: String
    public var members: Set<MemberKey>
}

/// What the sidebar is showing: one space, the sessions in none, or everything.
public enum SpaceView: Hashable, Codable, Sendable {
    case space(UUID)
    case unsorted
    case all
}

/// Every space, in the order the space bar shows them. A session is in at most one.
public struct SpaceBook: Codable, Equatable, Sendable {
    public private(set) var spaces: [Space] = []

    public init() {}

    public func space(_ id: UUID) -> Space? {
        spaces.first { $0.id == id }
    }

    public func space(of key: MemberKey) -> UUID? {
        spaces.first { $0.members.contains(key) }?.id
    }

    public func contains(_ view: SpaceView, _ key: MemberKey) -> Bool {
        switch view {
        case .all: return true
        case .unsorted: return space(of: key) == nil
        case let .space(id): return space(id)?.members.contains(key) ?? false
        }
    }

    @discardableResult
    public mutating func create(name: String) -> UUID {
        let id = UUID()
        spaces.append(Space(id: id, name: name, members: []))
        return id
    }

    public mutating func rename(_ id: UUID, to name: String) {
        guard let i = spaces.firstIndex(where: { $0.id == id }) else { return }
        spaces[i].name = name
    }

    /// Its sessions fall back to Unsorted; nothing is killed.
    public mutating func delete(_ id: UUID) {
        spaces.removeAll { $0.id == id }
    }

    /// `nil` moves them out of every space, into Unsorted.
    public mutating func move<S: Sequence>(_ keys: S, to id: UUID?) where S.Element == MemberKey {
        let keys = Set(keys)
        for i in spaces.indices {
            spaces[i].members.subtract(keys)
        }
        guard let id, let i = spaces.firstIndex(where: { $0.id == id }) else { return }
        spaces[i].members.formUnion(keys)
    }

    /// The space of that name, made if there is none, so naming a space twice never
    /// leaves two chips that look the same.
    public mutating func space(named name: String) -> UUID {
        spaces.first { $0.name == name }?.id ?? create(name: name)
    }

    public func has(_ view: SpaceView) -> Bool {
        if case let .space(id) = view { return space(id) != nil }
        return true
    }

    /// The space bar's chips in order: every space, then Unsorted while it holds anything
    /// or is the one showing, then All. With no spaces only All is left, which is the
    /// sidebar as it was before there were any.
    public func bar(showing: SpaceView, unsorted: Int) -> [SpaceView] {
        var out = spaces.map { SpaceView.space($0.id) }
        if !spaces.isEmpty, showing == .unsorted || unsorted > 0 {
            out.append(.unsorted)
        }
        return out + [.all]
    }

    /// Where the sidebar lands after the space showing is deleted: Unsorted, where its
    /// sessions just went, unless there is nothing there to show.
    public func landing(unsorted: Int) -> SpaceView {
        !spaces.isEmpty && unsorted > 0 ? .unsorted : .all
    }

    /// Where sessions living in `homes` can be moved: every space but the one they all
    /// already share, then Unsorted (`nil`) unless that is where all of them are.
    public func targets(from homes: Set<UUID?>) -> [UUID?] {
        var out: [UUID?] = spaces.map(\.id).filter { homes != [$0] }
        if homes != [nil] { out.append(nil) }
        return out
    }

    /// Forgets `vm`'s sessions that are no longer in its list. `alive` must be a complete
    /// `Sessions` frame: a VM that is merely offline still has every session it had.
    public mutating func prune(vm: String, alive: Set<String>) {
        for i in spaces.indices {
            spaces[i].members = spaces[i].members.filter { $0.vm != vm || alive.contains($0.id) }
        }
    }
}

extension SpaceView {
    /// `delta` chips along `bar`, wrapping at either end.
    public func stepped(by delta: Int, in bar: [SpaceView]) -> SpaceView {
        guard !bar.isEmpty else { return self }
        let i = bar.firstIndex(of: self) ?? 0
        let n = bar.count
        return bar[((i + delta) % n + n) % n]
    }
}

/// Sessions asked for while a space was showing. `Create` answers with nothing but the
/// next `Sessions` frame, so a new id is recognised by its VM, agent and name, oldest
/// request first.
///
/// ponytail: a same-named create from another client in the same instant could take the
/// slot. Have `Create` echo the new id if two clients ever race like that.
public struct PendingCreates: Sendable {
    private var waiting: [(vm: Int, agent: String, name: String, space: UUID)] = []

    public init() {}

    public mutating func expect(vm: Int, agent: String, name: String, space: UUID) {
        waiting.append((vm, agent, name, space))
    }

    /// The space each of `fresh`, the ids `vm` has just started listing, was asked for in.
    public mutating func claim(vm: Int, _ fresh: [SessionInfo]) -> [(id: String, space: UUID)] {
        fresh.filter { !$0.isShell }.compactMap { s in
            guard let i = waiting.firstIndex(where: {
                $0.vm == vm && $0.agent == s.agent && $0.name == s.name
            }) else { return nil }
            return (s.id, waiting.remove(at: i).space)
        }
    }
}

/// How much a session, or a whole space, wants you. Ordered by urgency, herdr's way: a
/// prompt beats everything, and a turn that finished unseen outranks one still running,
/// because the running one does not need you yet.
public enum Mood: Int, Comparable, Sendable {
    case empty
    case offline
    case stopped
    case idle
    case working
    case done
    case blocked

    public init(online: Bool, status: Status, activity: Activity, done: Bool) {
        if !online {
            self = .offline
        } else if status == .stopped {
            self = .stopped
        } else {
            switch activity {
            case .blocked: self = .blocked
            case .working: self = .working
            case .idle, .unknown: self = done ? .done : .idle
            }
        }
    }

    public static func < (a: Mood, b: Mood) -> Bool { a.rawValue < b.rawValue }

    public static func rollup<S: Sequence>(_ moods: S) -> Mood where S.Element == Mood {
        moods.max() ?? .empty
    }
}
