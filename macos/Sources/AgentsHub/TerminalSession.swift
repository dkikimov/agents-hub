import AgentsHubCore
import Combine
import Foundation
import GhosttyTerminal

/// Routes PTY bytes from the link queues to the right terminal without ever touching the
/// main actor. `AppModel` owns the terminals; this owns just enough to feed them, because
/// hopping every 8 KB chunk onto the main actor is what makes a busy agent stutter.
final class TerminalRouter: @unchecked Sendable {
    private let lock = NSLock()
    private var sinks: [SessionKey: AttachFeed] = [:]
    private var watch = ActivityWatch()

    func set(_ key: SessionKey, _ sink: AttachFeed?) {
        lock.lock()
        defer { lock.unlock() }
        sinks[key] = sink
        if sink == nil { watch.forget(key) }
    }

    func poked(_ key: SessionKey) {
        lock.lock()
        defer { lock.unlock() }
        watch.poked(key)
    }

    func deliver(_ key: SessionKey, _ data: Data, live: Bool) {
        lock.lock()
        let sink = sinks[key]
        // Replay is history, not activity — a reconnect must not light up every dot.
        if live { watch.output(key) }
        lock.unlock()
        sink?.receive(data)
    }

    func finish(_ key: SessionKey, code: Int32) {
        lock.lock()
        let sink = sinks[key]
        lock.unlock()
        sink?.finish(exitCode: UInt32(bitPattern: code))
    }

    /// Keys that produced output of their own within `window`.
    func active(within window: TimeInterval) -> Set<SessionKey> {
        lock.lock()
        defer { lock.unlock() }
        return watch.active(within: window)
    }
}

/// One session's terminal: the ghostty-side session, the view state that renders it, and
/// the feed between them and the link. Created fresh on attach — handing `backend` a
/// different `InMemoryTerminalSession` instance is what rebuilds the surface, which is
/// exactly the teardown a restart needs.
@MainActor
final class TerminalSession {
    let key: SessionKey
    let state = TerminalViewState(controller: Ghostty.controller)
    let session: InMemoryTerminalSession
    let feed = AttachFeed()

    private var bellsAtReplayEnd: Int?
    private var layout: AnyCancellable?

    /// Bells rung since the attach replay finished.
    ///
    /// Replay is real scrollback fed to a real emulator, so a bell buried in it rings
    /// exactly like a live one — counting from zero would fire a notification for every
    /// answer already read, on every reconnect and every restart. Same hazard as the
    /// feed's writeback, so it is drawn from the same line.
    var liveBells: Int {
        guard let bellsAtReplayEnd else { return 0 }
        return max(0, state.bellCount - bellsAtReplayEnd)
    }

    /// `onGrid` reports the *real* grid ghostty laid out. It is the only trustworthy
    /// source: the cell size depends on the user's font, which comes from their own
    /// ghostty config, so anything derived from view geometry here would be a guess that
    /// fights the surface and leaves the guest wrapping at the wrong width.
    init(key: SessionKey,
         send: @escaping @Sendable (Req) -> Void,
         onGrid: @escaping @Sendable (UInt16, UInt16) -> Void) {
        self.key = key
        let id = key.id
        let feed = self.feed
        let report: @Sendable (AttachFeed.Grid) -> Void = { grid in
            send(.resize(id: id, cols: grid.cols, rows: grid.rows))
            onGrid(grid.cols, grid.rows)
        }
        session = InMemoryTerminalSession(
            write: { data in
                guard feed.allowsWriteback(data) else { return }
                send(.input(id: id, data: data))
            },
            resize: { viewport in
                let grid = AttachFeed.Grid(cols: viewport.columns, rows: viewport.rows)
                if feed.resized(grid) { report(grid) }
            }
        )
        let session = self.session
        feed.bind(
            parse: { session.receive($0) },
            exit: { session.finish(exitCode: $0, runtimeMilliseconds: 0) },
            // ponytail: a fixed beat for ghostty's IO thread to take the terminal lock it
            // resizes under, since nothing reports the resize applied. Queued behind it,
            // the write waits; a thread stalled past the beat still lays out narrow.
            sized: {
                DispatchQueue.global().asyncAfter(deadline: .now() + .milliseconds(20)) {
                    feed.release()
                }
            },
            settled: { Task { @MainActor [weak self] in await self?.replayParsed() } }
        )
        layout = state.$surfaceSize.compactMap { $0 }.sink { size in
            let grid = AttachFeed.Grid(cols: size.columns, rows: size.rows)
            if feed.laidOut(grid) { report(grid) }
        }
        state.configuration = TerminalSurfaceOptions(
            backend: .inMemory(session),
            // Claude Code repaints fully on every resize; coalescing a live window drag
            // is the difference between smooth and unusable.
            resizeThrottleMilliseconds: 100
        )
        state.isSurfaceVisible = false
    }

    func focus() {
        // requestFocus is the sanctioned imperative path; the FocusState bridge is
        // unreliable across tab switches and the package says so.
        state.requestFocus()
    }

    /// On the main actor, never detached: a replay dense with titles and OSC 7 (any shell
    /// prompt) fills ghostty's 64-slot app mailbox, only a main-thread tick empties it, and
    /// only a main-thread caller ticks while it waits. Off main, writeback stays shut —
    /// dropping keystrokes — until something else happens to tick.
    private func replayParsed() async {
        session.waitForPendingOutput()
        // ponytail: a fixed grace, because answers to parsed queries leave through
        // ghostty's IO-thread mailbox after the parse returns, and nothing reports that
        // queue empty. Measured at under a millisecond; an IO thread stalled longer still
        // leaks them. The fix at the origin is the fork's
        // `ghostty_surface_write_buffer_replay`, once libghostty-spm's session exposes it.
        try? await Task.sleep(for: .milliseconds(100))
        let dropped = feed.openWriteback()
        if dropped > 0 {
            NSLog("agents-hub: dropped %d bytes of replay writeback for %@", dropped, key.id)
        }
        // A turn late, deliberately. Replay's bells are parsed by the time
        // `waitForPendingOutput` returns but publish through `terminalRunOnMainNextTurn`,
        // so they are sitting on the main queue ahead of this — which is FIFO, so reading
        // the count here counts all of them.
        DispatchQueue.main.async { [weak self] in
            guard let self else { return }
            MainActor.assumeIsolated { self.bellsAtReplayEnd = self.state.bellCount }
        }
    }
}
