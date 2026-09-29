import Foundation

/// Stands between the link and ghostty for one attach, because a real emulator fed
/// replayed history gets two things wrong that `vt100::Parser` never did.
///
/// It lays history out at the wrong width. A surface starts at a default grid (49×17) and
/// is resized a beat later, and ghostty parses whatever it already holds in between; an
/// alt-screen agent's history is never reflowed, so a stopped one stays garbled. Bytes are
/// held until ghostty reports the grid the view laid out, and the default grid's resize is
/// withheld too — sent on, it squeezes every session to 49 columns and back each time a
/// surface is built, and the log keeps the narrow redraw.
///
/// And it answers the DA/XTWINOPS queries buried in history, which would reach the live
/// PTY as if typed — a shell prints them at its prompt. Writeback stays shut until the
/// replay has arrived and been released, and the host has let ghostty finish parsing it.
public final class AttachFeed: @unchecked Sendable {
    public struct Grid: Equatable, Sendable {
        public let cols: UInt16
        public let rows: UInt16

        public init(cols: UInt16, rows: UInt16) {
            self.cols = cols
            self.rows = rows
        }
    }

    /// Matches libghostty-spm's own surfaceless buffer: oldest bytes go first.
    static let backlogLimit = 1 << 20

    private let lock = NSLock()
    private var parse: (@Sendable (Data) -> Void)?
    private var exit: (@Sendable (UInt32) -> Void)?
    private var onSized: (@Sendable () -> Void)?
    private var onSettled: (@Sendable () -> Void)?
    /// Nil once released; from then on bytes go straight to `parse`.
    private var backlog: Data? = Data()
    private var backlogExit: UInt32?
    private var laidOut: Grid?
    private var reported: Grid?
    /// Latched: once ghostty has reached the laid-out grid, later sizes are a drag, not
    /// the default.
    private var sized = false
    private var arrived = false
    private var writebackOpen = false
    private var dropped = 0

    public init() {}

    /// Separate from `init` because the terminal's own callbacks need the feed first.
    /// `sized` fires once, when ghostty has reported the laid-out grid; the host answers
    /// with `release`. `settled` fires once, when the replay has arrived and been released.
    public func bind(
        parse: @escaping @Sendable (Data) -> Void,
        exit: @escaping @Sendable (UInt32) -> Void,
        sized: @escaping @Sendable () -> Void,
        settled: @escaping @Sendable () -> Void
    ) {
        lock.lock()
        self.parse = parse
        self.exit = exit
        onSized = sized
        onSettled = settled
        lock.unlock()
    }

    /// PTY bytes from the link, replay or live — whichever comes first proves the attach
    /// has answered, since the daemon sends replay ahead of everything else.
    public func receive(_ data: Data) {
        lock.lock()
        arrived = true
        if backlog != nil {
            backlog!.append(data)
            let excess = backlog!.count - Self.backlogLimit
            if excess > 0 { backlog!.removeFirst(excess) }
        } else {
            parse?(data)
        }
        settle()
    }

    public func finish(exitCode: UInt32) {
        lock.lock()
        arrived = true
        if backlog != nil {
            backlogExit = exitCode
        } else {
            exit?(exitCode)
        }
        settle()
    }

    /// Ghostty's IO thread, just before it resizes the grid. False for anything ahead of
    /// the laid-out grid, which the PTY must never see.
    public func resized(_ grid: Grid) -> Bool {
        lock.lock()
        reported = grid
        let matched = matchIfLaidOut()
        let forward = sized
        lock.unlock()
        if matched { onSized?() }
        return forward
    }

    /// The grid the view laid out, from the main actor. True only if this is what matched
    /// it: ghostty reported that grid first and `resized` withheld it, so the caller has to
    /// send it on.
    public func laidOut(_ grid: Grid) -> Bool {
        lock.lock()
        laidOut = grid
        let matched = matchIfLaidOut()
        lock.unlock()
        if matched { onSized?() }
        return matched
    }

    /// Hands the held bytes to `parse`. Never called from `resized` itself: ghostty reports
    /// a size before it takes the terminal lock to apply it, and a write that wins that
    /// race is laid out at the old grid.
    public func release() {
        lock.lock()
        guard let held = backlog else {
            lock.unlock()
            return
        }
        if !held.isEmpty { parse?(held) }
        if let backlogExit { exit?(backlogExit) }
        backlog = nil
        backlogExit = nil
        settle()
    }

    public func allowsWriteback(_ data: Data) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if writebackOpen { return true }
        dropped += data.count
        return false
    }

    /// Returns how many bytes were dropped, for the log.
    @discardableResult
    public func openWriteback() -> Int {
        lock.lock()
        defer { lock.unlock() }
        writebackOpen = true
        return dropped
    }

    /// True the one time ghostty's grid first reaches the laid-out one. Lock held.
    private func matchIfLaidOut() -> Bool {
        guard !sized, let reported, reported == laidOut else { return false }
        sized = true
        return true
    }

    /// Lock held on entry, released on exit, so `onSettled` runs outside it.
    private func settle() {
        var fire: (@Sendable () -> Void)?
        if arrived, backlog == nil {
            fire = onSettled
            onSettled = nil
        }
        lock.unlock()
        fire?()
    }
}
