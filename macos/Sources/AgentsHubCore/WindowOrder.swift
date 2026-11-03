/// Which window the sessions take their size from: the one used last, as tmux's
/// `window-size latest`.
///
/// A PTY has one size, and every window has a surface of its own for each session, so
/// exactly one window can speak for them. Its grid is what every PTY is kept at, and only
/// its surfaces may write to a PTY — each surface answers the guest's DA/XTWINOPS queries,
/// and two windows would type every answer twice. The rest draw the same output at their
/// own size, which wraps wrong until they are used: the "fake borders" of a tmux session
/// shown in two terminals.
public struct WindowOrder<ID: Hashable> {
    /// Least recently used first.
    public private(set) var recent: [ID] = []

    public init() {}

    public var owner: ID? { recent.last }

    /// With no window at all there is nobody to disagree with.
    public func owns(_ id: ID) -> Bool {
        owner.map { $0 == id } ?? true
    }

    /// A window just opened goes to the back: the one in use keeps the sessions until
    /// the new one is actually used — which, for a window opened from the menu, is at
    /// once, since it opens key.
    public mutating func opened(_ id: ID) {
        guard !recent.contains(id) else { return }
        recent.insert(id, at: 0)
    }

    /// Key, or typed into. True when the sessions move to it, which is when every PTY has
    /// to take its size.
    public mutating func used(_ id: ID) -> Bool {
        let was = owner
        recent.removeAll { $0 == id }
        recent.append(id)
        return was != id
    }

    /// The window that takes over, if the closed one had the sessions and any is left.
    public mutating func closed(_ id: ID) -> ID? {
        let was = owner
        recent.removeAll { $0 == id }
        return was == id ? owner : nil
    }
}
