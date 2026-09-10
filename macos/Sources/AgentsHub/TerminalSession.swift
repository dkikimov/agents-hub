import AgentsHubCore
import Foundation
import GhosttyTerminal

/// Holds back anything ghostty writes while the attach replay is still being parsed.
///
/// Ghostty is a real emulator, so feeding it 128 KB of replayed history could make it
/// *answer* the DA/DSR/CPR queries buried in that history — and those answers would go
/// out the write callback into the live PTY as if typed. `vt100::Parser` never talked
/// back, so the Rust client never had to care.
///
/// Measured against real Claude Code scrollback this never actually fires, so it is
/// insurance rather than load-bearing. It costs a lock on a path that runs at keystroke
/// rate, which is free.
final class ReplayGate: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    private var held = 0

    func allows(_ data: Data) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if open { return true }
        held += data.count
        return false
    }

    /// Returns how many bytes were suppressed, for the log.
    @discardableResult
    func openUp() -> Int {
        lock.lock()
        defer { lock.unlock() }
        open = true
        return held
    }
}

/// Routes PTY bytes from the link queues to the right terminal without ever touching the
/// main actor. `AppModel` owns the terminals; this owns just enough to feed them, because
/// hopping every 8 KB chunk onto the main actor is what makes a busy agent stutter.
final class TerminalRouter: @unchecked Sendable {
    private let lock = NSLock()
    private var sinks: [SessionKey: InMemoryTerminalSession] = [:]
    private var lastOutput: [SessionKey: Date] = [:]

    func set(_ key: SessionKey, _ sink: InMemoryTerminalSession?) {
        lock.lock()
        defer { lock.unlock() }
        sinks[key] = sink
        if sink == nil { lastOutput[key] = nil }
    }

    func deliver(_ key: SessionKey, _ data: Data, live: Bool) {
        lock.lock()
        let sink = sinks[key]
        // Replay is history, not activity — a reconnect must not light up every dot.
        if live { lastOutput[key] = Date() }
        lock.unlock()
        sink?.receive(data)
    }

    func finish(_ key: SessionKey, code: Int32) {
        lock.lock()
        let sink = sinks[key]
        lock.unlock()
        sink?.finish(exitCode: UInt32(bitPattern: code), runtimeMilliseconds: 0)
    }

    /// Keys that produced output within `window`.
    func active(within window: TimeInterval) -> Set<SessionKey> {
        lock.lock()
        defer { lock.unlock() }
        let cutoff = Date().addingTimeInterval(-window)
        return Set(lastOutput.filter { $0.value > cutoff }.keys)
    }
}

/// One session's terminal: the ghostty-side session, the view state that renders it, and
/// the replay gate. Created fresh on attach — handing `backend` a different
/// `InMemoryTerminalSession` instance is what rebuilds the surface, which is exactly the
/// teardown a restart needs.
@MainActor
final class TerminalSession {
    let key: SessionKey
    let state = TerminalViewState(controller: Ghostty.controller)
    let session: InMemoryTerminalSession

    private let gate = ReplayGate()

    /// `onGrid` reports the *real* grid ghostty laid out. It is the only trustworthy
    /// source: the cell size depends on the user's font, which comes from their own
    /// ghostty config, so anything derived from view geometry here would be a guess that
    /// fights the surface and leaves the guest wrapping at the wrong width.
    init(key: SessionKey,
         send: @escaping @Sendable (Req) -> Void,
         onGrid: @escaping @Sendable (UInt16, UInt16) -> Void) {
        self.key = key
        let id = key.id
        let gate = self.gate
        session = InMemoryTerminalSession(
            write: { data in
                guard gate.allows(data) else { return }
                send(.input(id: id, data: data))
            },
            resize: { viewport in
                send(.resize(id: id, cols: viewport.columns, rows: viewport.rows))
                onGrid(viewport.columns, viewport.rows)
            }
        )
        state.configuration = TerminalSurfaceOptions(
            backend: .inMemory(session),
            // Claude Code repaints fully on every resize; coalescing a live window drag
            // is the difference between smooth and unusable.
            resizeThrottleMilliseconds: 100
        )
        state.isSurfaceVisible = false
        openGateOnceReplayDrains()
    }

    func focus() {
        // requestFocus is the sanctioned imperative path; the FocusState bridge is
        // unreliable across tab switches and the package says so.
        state.requestFocus()
    }

    /// ponytail: polls for the surface because `TerminalViewState` is the view's own
    /// delegate and there is no attach callback to hang this on. Replace if the package
    /// ever exposes one.
    private func openGateOnceReplayDrains() {
        let session = self.session
        let gate = self.gate
        let name = key.id
        Task { @MainActor [weak self] in
            for _ in 0..<600 where self?.state.surface == nil {
                try? await Task.sleep(for: .milliseconds(16))
            }
            await Task.detached { session.waitForPendingOutput() }.value
            let held = gate.openUp()
            if held > 0 {
                NSLog("agents-hub: held %d bytes of replay writeback for %@", held, name)
            }
        }
    }
}
