import Testing

@testable import AgentsHubCore

/// What `app.rs::fixture` plus the `event.rs` assertions were for, except the registry
/// returns its effects instead of sending them, so there is no channel to drain.
@Suite struct SessionRegistryTests {
    private func info(_ id: String, _ status: Status) -> SessionInfo {
        SessionInfo(id: id, agent: "claude", name: id, cwd: "~/p", status: status, createdAt: 0)
    }

    private func key(_ id: String, vm: Int = 0) -> SessionKey { SessionKey(vm: vm, id: id) }

    @Test func firstConnectListsAndThenAttachesEverything() {
        var r = SessionRegistry()
        #expect(r.connected(vm: 0) == [.send(vm: 0, req: .list)])

        let effects = r.sessions(vm: 0, [info("a", .running), info("b", .stopped)],
                                 cols: 80, rows: 24)
        #expect(effects == [
            .teardown(key("a")), .send(vm: 0, req: .attach(id: "a", cols: 80, rows: 24)),
            .teardown(key("b")), .send(vm: 0, req: .attach(id: "b", cols: 80, rows: 24)),
        ])
        #expect(r.attached == [key("a"), key("b")])
    }

    /// `Sessions` frames arrive unsolicited on every daemon-side change, so a frame that
    /// says nothing new must do nothing at all.
    @Test func anUnchangedFrameIsIdempotent() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        let list = [info("a", .running)]
        _ = r.sessions(vm: 0, list, cols: 80, rows: 24)
        #expect(r.sessions(vm: 0, list, cols: 80, rows: 24).isEmpty)
        #expect(r.sessions(vm: 0, list, cols: 80, rows: 24).isEmpty)
    }

    /// Restart reuses the id, but the old broadcast channel died with the old PTY. A
    /// surviving attachment would go silent forever — this is the bug event.rs:576 fixes.
    @Test func stoppedToRunningReattachesUnderTheSameId() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.sessions(vm: 0, [info("a", .stopped)], cols: 80, rows: 24)

        #expect(r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24) == [
            .teardown(key("a")), .send(vm: 0, req: .attach(id: "a", cols: 80, rows: 24)),
        ])
    }

    /// The reverse is not a restart: a session that merely died keeps its terminal, so
    /// its scrollback stays readable.
    @Test func runningToStoppedDoesNotReattach() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24)
        #expect(r.sessions(vm: 0, [info("a", .stopped)], cols: 80, rows: 24).isEmpty)
    }

    @Test func aVanishedSessionIsForgotten() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.sessions(vm: 0, [info("a", .running), info("b", .running)], cols: 80, rows: 24)

        #expect(r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24) == [.forget(key("b"))])
        #expect(r.attached == [key("a")])
    }

    /// The daemon has no memory of a dropped connection's subscriptions, so every
    /// attachment for that VM is void — and only for that VM.
    @Test func reconnectVoidsOnlyThatVMsAttachments() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.connected(vm: 1)
        _ = r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24)
        _ = r.sessions(vm: 1, [info("z", .running)], cols: 80, rows: 24)

        #expect(r.connected(vm: 0) == [.teardown(key("a")), .send(vm: 0, req: .list)])
        #expect(r.attached == [key("z", vm: 1)], "vm 1 was never touched")

        // And the frame that comes back re-attaches from scratch.
        #expect(r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24) == [
            .teardown(key("a")), .send(vm: 0, req: .attach(id: "a", cols: 80, rows: 24)),
        ])
    }

    @Test func disconnectVoidsWithoutSending() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24)
        #expect(r.disconnected(vm: 0) == [.teardown(key("a"))])
        #expect(r.attached.isEmpty)
    }

    /// Unmounted sessions have no surface to report their own size, so they must be
    /// resized explicitly or their PTY stays at 80×24 forever.
    @Test func resizeReachesEveryAttachedSessionOnEveryVM() {
        var r = SessionRegistry()
        _ = r.connected(vm: 0)
        _ = r.connected(vm: 1)
        _ = r.sessions(vm: 0, [info("a", .running)], cols: 80, rows: 24)
        _ = r.sessions(vm: 1, [info("z", .running)], cols: 80, rows: 24)

        #expect(r.resized(cols: 200, rows: 50) == [
            .send(vm: 0, req: .resize(id: "a", cols: 200, rows: 50)),
            .send(vm: 1, req: .resize(id: "z", cols: 200, rows: 50)),
        ])
    }
}

@Suite struct LineReaderTests {
    private func collect(_ chunks: [[UInt8]]) -> [String] {
        var r = LineReader()
        var out: [String] = []
        for c in chunks {
            r.push(c) { out.append(String(decoding: $0, as: UTF8.self)) }
        }
        return out
    }

    @Test func splitsOnNewlinesAndDropsBlanks() {
        #expect(collect([Array("a\nb\n\nc\n".utf8)]) == ["a", "b", "c"])
    }

    @Test func aLineSplitAcrossChunksIsRejoined() {
        #expect(collect([Array("he".utf8), Array("ll".utf8), Array("o\n".utf8)]) == ["hello"])
    }

    @Test func oneByteAtATimeIsStillCorrect() {
        let chunks = Array("one\ntwo\n".utf8).map { [$0] }
        #expect(collect(chunks) == ["one", "two"])
    }

    /// The frame that actually matters: a 128 KB attach replay is ~175 KB of base64 on a
    /// single line, arriving in whatever pieces the pipe hands over.
    @Test func aReplaySizedLineSurvivesArrivingInPieces() {
        let payload = String(repeating: "x", count: 200_000)
        var chunks = Array((payload + "\n").utf8).chunked(into: 4096)
        chunks.append([])
        #expect(collect(chunks) == [payload])
    }

    @Test func aPeerThatNeverSendsANewlineIsRefused() {
        var r = LineReader()
        var ok = true
        // One byte over the cap, in big steps.
        for _ in 0...(LineReader.maxLine / 65536) {
            ok = r.push([UInt8](repeating: 0x41, count: 65536)) { _ in }
        }
        #expect(ok == false)
    }
}

extension Array {
    func chunked(into size: Int) -> [[Element]] {
        stride(from: 0, to: count, by: size).map { Array(self[$0..<Swift.min($0 + size, count)]) }
    }
}
