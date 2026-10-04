/// What each agent's dot says, and when it is worth a banner.
///
/// The daemon reads working, blocked and idle off the agent's screen (`detect.rs`); this
/// adds the one thing only a client can know, which is whether you have seen it. herdr's
/// rule: a turn that ends, or a prompt that gets answered, while you are looking
/// elsewhere is *done* until you look. An idle agent on first sight is just how it was
/// left, so a reconnect never lights every dot or fires a banner per session.
public struct AgentWatch {
    public enum News: Equatable, Sendable {
        /// Entered a permission prompt or a question.
        case needsYou
        /// Went idle from working or blocked.
        case finished
    }

    private var last: [SessionKey: Activity] = [:]
    public private(set) var done: Set<SessionKey> = []

    public init() {}

    /// One session's reading, from a `Sessions` frame. `watching` is the session in
    /// front of you, whose news you already have.
    public mutating func update(_ key: SessionKey, _ activity: Activity,
                                watching: Bool) -> News? {
        let was = last.updateValue(activity, forKey: key)
        guard let was, was != activity else { return nil }
        switch activity {
        case .idle where was == .working || was == .blocked:
            guard !watching else { return nil }
            done.insert(key)
            return .finished
        case .idle:
            return nil
        case .blocked:
            done.remove(key)
            return watching ? nil : .needsYou
        case .working, .unknown:
            done.remove(key)
            return nil
        }
    }

    public mutating func seen(_ key: SessionKey) {
        done.remove(key)
    }

    public mutating func forget(_ key: SessionKey) {
        last[key] = nil
        done.remove(key)
    }
}
