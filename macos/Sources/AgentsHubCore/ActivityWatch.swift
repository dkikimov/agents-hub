import Foundation

/// Which sessions are working, from the bytes they print.
///
/// The naive reading — anything printed recently — lights every dot at once whenever the
/// client pokes every session at once, and it does that often: a focus report to each
/// mounted surface when the window loses key, and a SIGWINCH to each PTY on every reattach
/// (`Req.attach` resizes server-side) and every pane resize. An agent repainting because we
/// nudged it is not an agent working.
///
/// So output is only activity if we did not just poke that session. A working agent keeps
/// printing past the grace and still lights up; a one-shot repaint we caused never does.
public struct ActivityWatch {
    /// ponytail: one fixed window instead of matching each repaint to the request that
    /// caused it. A dot is decoration, and a slow remote that repaints later than this only
    /// costs a stray blink. Pair each poke with its reply if that ever stops being true.
    public static let grace: TimeInterval = 0.5

    private var lastOutput: [SessionKey: Date] = [:]
    private var lastPoke: [SessionKey: Date] = [:]

    public init() {}

    /// Call for every request sent at `key` — the reply is ours, not the agent's.
    public mutating func poked(_ key: SessionKey, at now: Date = Date()) {
        lastPoke[key] = now
    }

    public mutating func output(_ key: SessionKey, at now: Date = Date()) {
        guard now.timeIntervalSince(lastPoke[key] ?? .distantPast) > Self.grace else { return }
        lastOutput[key] = now
    }

    public mutating func forget(_ key: SessionKey) {
        lastOutput[key] = nil
        lastPoke[key] = nil
    }

    public func active(within window: TimeInterval, now: Date = Date()) -> Set<SessionKey> {
        let cutoff = now.addingTimeInterval(-window)
        return Set(lastOutput.filter { $0.value > cutoff }.keys)
    }
}
