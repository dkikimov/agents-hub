/// Remembers which bells the user has already been told about.
///
/// Takes a count of *live* bells — a terminal that has not finished its attach replay
/// reports zero, because replayed scrollback rings real bells. Separate from the app so the
/// rule that is easy to get wrong is testable without a surface: a restart hands over a
/// fresh terminal counting from zero, and treating that drop as "no rise" would leave the
/// session silent for the rest of the run.
public struct BellWatch {
    private var seen: [SessionKey: Int] = [:]

    public init() {}

    public mutating func rang(_ key: SessionKey, count: Int) -> Bool {
        defer { seen[key] = count }
        return count > seen[key] ?? 0
    }

    public mutating func forget(_ key: SessionKey) {
        seen[key] = nil
    }
}
