import GhosttyTerminal
import SwiftUI

/// Risk probe. Ghostty is a real emulator, so feeding it 128 KB of replayed history makes
/// it *answer* the DA/DSR/CPR/OSC queries buried in that history — and those answers would
/// go out the write callback into the live PTY as if typed. `vt100::Parser` never talked
/// back, so the Rust client never had to care. This holds them and reports what it caught.
final class ReplayGate: @unchecked Sendable {
    private let lock = NSLock()
    private var open = false
    private var held: [Data] = []

    func allows(_ d: Data) -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if open { return true }
        held.append(d)
        return false
    }

    func openUp() -> [Data] {
        lock.lock()
        defer { lock.unlock() }
        open = true
        let caught = held
        held = []
        return caught
    }
}

@MainActor
final class Spike: ObservableObject {
    @Published var status = "connecting…"

    let state = TerminalViewState()
    private let link = Link()
    private let gate = ReplayGate()
    private var session: InMemoryTerminalSession?
    private var attached: String?
    private var replayBytes = 0
    private var liveBytes = 0

    private let host = ProcessInfo.processInfo.environment["AH_HOST"].flatMap { $0.isEmpty ? nil : $0 }
    private let want = ProcessInfo.processInfo.environment["AH_SESSION"] ?? ""

    func start() {
        link.onResp = { [weak self] resp in
            Task { @MainActor in self?.on(resp) }
        }
        do {
            try link.start(host: host)
        } catch {
            status = "spawn failed: \(error)"
            return
        }
        link.send(.list)
        status = "listing sessions on \(host ?? "local")…"
    }

    private func on(_ resp: Resp) {
        switch resp {
        case let .sessions(list):
            guard attached == nil else { return }  // spike attaches once
            guard let pick = choose(from: list) else {
                status = "no session matched AH_SESSION=\(want)"
                return
            }
            attach(pick)
        case let .output(id, data, live):
            guard id == attached else { return }
            if live { liveBytes += data.count } else { replayBytes += data.count }
            session?.receive(data)
            status = "\(status.prefix(while: { $0 != "·" }))· replay \(replayBytes)B · live \(liveBytes)B"
        case let .exited(id, code):
            guard id == attached else { return }
            session?.finish(exitCode: UInt32(bitPattern: code), runtimeMilliseconds: 0)
            status += " · exited(\(code))"
        case let .error(msg):
            log("daemon error: \(msg)")
            status = "daemon error: \(msg)"
        case .dirs:
            break
        }
    }

    private func choose(from list: [SessionInfo]) -> SessionInfo? {
        log("sessions: " + list.map { "\($0.status)/\($0.agent)/\($0.name)/\($0.id)" }.joined(separator: "  "))
        if want.isEmpty { return list.first { $0.status == "Running" } ?? list.first }
        return list.first { $0.id == want }
            ?? list.first { $0.name.contains(want) || $0.agent.contains(want) }
    }

    private func attach(_ info: SessionInfo) {
        attached = info.id
        let id = info.id

        let session = InMemoryTerminalSession(
            write: { [gate, link] data in
                log("write -> \(escaped(data))")
                guard gate.allows(data) else { return }
                link.send(.input(id: id, data: data))
            },
            resize: { [link] viewport in
                link.send(.resize(id: id, cols: viewport.columns, rows: viewport.rows))
            }
        )
        self.session = session
        state.configuration = TerminalSurfaceOptions(
            backend: .inMemory(session),
            resizeThrottleMilliseconds: 100
        )
        status = "\(info.agent) · \(info.name) [\(info.status)] "
        link.send(.attach(id: id, cols: 120, rows: 32))
        state.requestFocus()

        openGateOnceReplayDrains(session)
    }

    /// ponytail: polls for the surface because TerminalViewState is the view's own
    /// delegate and there is no attach callback to hang this on. Replace if the package
    /// ever exposes one.
    private func openGateOnceReplayDrains(_ session: InMemoryTerminalSession) {
        Task { @MainActor in
            for _ in 0..<300 where state.surface == nil {
                try? await Task.sleep(for: .milliseconds(16))
            }
            await Task.detached { session.waitForPendingOutput() }.value
            try? await Task.sleep(for: .milliseconds(250))
            let caught = gate.openUp()
            if caught.isEmpty {
                log("WRITEBACK: nothing held back during replay — risk #1 does not bite here")
            } else {
                let total = caught.reduce(0) { $0 + $1.count }
                log("WRITEBACK: held \(caught.count) write(s), \(total) bytes, that would have "
                    + "been typed into the live PTY:")
                for d in caught { log("   \(escaped(d))") }
            }
            status += " · gate open"

            // Round-trip the *real* input path — through ghostty, not around it — so
            // this proves key encoding and Req::Input, not just the transport.
            if let probe = ProcessInfo.processInfo.environment["AH_PROBE"], !probe.isEmpty {
                // Focus is not optional: paste() returns false without a focused surface,
                // which is why the first run of this probe silently sent nothing.
                // ignoringOtherApps steals the user's keystrokes mid-probe — only ever
                // do this in the spike, and only against a throwaway session.
                NSApp.activate()
                state.requestFocus()
                try? await Task.sleep(for: .milliseconds(300))
                let ok = state.paste(text: probe + "\n")
                log("PROBE: paste(\(probe.debugDescription)) -> \(ok); surface=\(state.surface != nil) focused=\(state.isFocused)")
                try? await Task.sleep(for: .milliseconds(2000))
            }

            // Proof that ghostty actually parsed and laid out the replay, and the
            // headless stand-in for `tmux capture-pane -p`.
            try? await Task.sleep(for: .milliseconds(400))
            if let text = session.readViewportText() {
                let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
                log("VIEWPORT: \(lines.count) rows rendered; last 12 non-empty:")
                for l in lines.filter({ !$0.trimmingCharacters(in: .whitespaces).isEmpty }).suffix(12) {
                    log("   | \(l)")
                }
            } else {
                log("VIEWPORT: nil — no surface, so nothing was rendered")
            }
        }
    }
}

func escaped(_ d: Data) -> String {
    d.map { b in
        switch b {
        case 0x1b: return "\\e"
        case 0x0a: return "\\n"
        case 0x0d: return "\\r"
        case 0x20...0x7e: return String(UnicodeScalar(b))
        default: return String(format: "\\x%02x", b)
        }
    }.joined()
}

@main
struct SpikeApp: App {
    @StateObject private var spike = Spike()

    var body: some Scene {
        WindowGroup("agents-hub spike") {
            VStack(spacing: 0) {
                Text(spike.status)
                    .font(.system(size: 11, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 8).padding(.vertical, 4)
                    .background(.quaternary)
                TerminalSurfaceView(context: spike.state)
            }
            .frame(minWidth: 980, minHeight: 620)
            .task { spike.start() }
        }
    }
}
