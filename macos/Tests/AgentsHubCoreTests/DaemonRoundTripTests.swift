import Foundation
import Testing

@testable import AgentsHubCore

/// The Swift twin of `tests/roundtrip.rs`, and the only test here that runs against real
/// bytes from a real daemon over a real PTY. Everything else checks the codec against
/// itself, which would happily agree on a shape Rust never emits — this is what catches
/// a renamed field, a base64 mismatch or a framing bug.
///
/// Never touches the real state dir: `AGENTS_HUB_DIR` and `AGENTS_HUB_CONFIG` are
/// scratch, and the daemon is one we started and terminate ourselves rather than
/// anything found running.
@Suite(.serialized) struct DaemonRoundTripTests {
    private actor FrameLog {
        private var frames: [Resp] = []
        private var up = false

        func record(_ event: LinkEvent) {
            switch event {
            case .up: up = true
            case .down: up = false
            case let .frame(f): frames.append(f)
            }
        }

        func waitForLink(timeout: TimeInterval) async -> Bool {
            await poll(timeout: timeout) { self.up } != nil
        }

        func first(timeout: TimeInterval = 25,
                   where predicate: @escaping (Resp) -> Bool) async -> Resp? {
            await poll(timeout: timeout) { self.frames.first(where: predicate) }
        }

        private func poll<T>(timeout: TimeInterval, _ probe: () -> T?) async -> T? {
            let deadline = Date().addingTimeInterval(timeout)
            while Date() < deadline {
                if let v = probe(), !((v as? Bool) == false) { return v }
                try? await Task.sleep(for: .milliseconds(40))
            }
            return nil
        }
    }

    private static func sessions(_ r: Resp) -> [SessionInfo]? {
        if case let .sessions(list) = r { return list }
        return nil
    }

    @Test func createDriveAndKillAgainstARealDaemon() async throws {
        let helper = Helper.url
        try #require(FileManager.default.isExecutableFile(atPath: helper.path),
                     "build the Rust binary first: cargo build")

        // Not NSTemporaryDirectory(): on macOS it is a ~50-character /var/folders path,
        // and `state/sock` past it blows sockaddr_un's 104-byte sun_path limit, which
        // the daemon reports as "path must be shorter than SUN_LEN".
        let root = URL(fileURLWithPath: "/tmp")
            .appendingPathComponent("ah-t-\(String(UUID().uuidString.prefix(8)))")
        let state = root.appendingPathComponent("state")
        let configPath = root.appendingPathComponent("config.toml")
        try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: root) }

        // A marker the PTY prints once, so "did output arrive" is unambiguous.
        try """
        [[vm]]
        name = "test"

        [agents.echo]
        command = ["sh", "-c", "echo ping-from-pty; sleep 30"]
        """.write(to: configPath, atomically: true, encoding: .utf8)

        let env = [
            "AGENTS_HUB_DIR": state.path,
            "AGENTS_HUB_CONFIG": configPath.path,
        ]

        // Start the daemon ourselves so `defer` can stop exactly this one. `stdio` would
        // otherwise spawn it, and there would be no handle to terminate.
        let daemonLog = root.appendingPathComponent("daemon.out")
        FileManager.default.createFile(atPath: daemonLog.path, contents: nil)
        let logHandle = try FileHandle(forWritingTo: daemonLog)

        let daemon = Process()
        daemon.executableURL = helper
        daemon.arguments = ["serve"]
        daemon.environment = ProcessInfo.processInfo.environment.merging(env) { _, new in new }
        daemon.standardOutput = logHandle
        daemon.standardError = logHandle
        try daemon.run()
        defer { daemon.terminate() }

        func daemonSaid() -> String {
            let own = (try? String(contentsOf: daemonLog, encoding: .utf8)) ?? ""
            let spawned = (try? String(contentsOf: state.appendingPathComponent("daemon.log"),
                                       encoding: .utf8)) ?? ""
            return "helper=\(helper.path)\n--- serve ---\n\(own)\n--- spawned ---\n\(spawned)"
        }

        // The socket is what `stdio` actually needs; wait for it rather than guessing.
        let sock = state.appendingPathComponent("sock")
        for _ in 0..<100 where !FileManager.default.fileExists(atPath: sock.path) {
            try await Task.sleep(for: .milliseconds(50))
        }
        #expect(FileManager.default.fileExists(atPath: sock.path),
                "daemon never bound its socket. \(daemonSaid())")

        let log = FrameLog()
        let link = VMLink(index: 0, vm: VMConfig(name: "test"), helper: helper, extraEnv: env)
        link.onEvent = { _, event in Task { await log.record(event) } }
        link.start()
        defer { link.stop() }

        #expect(await log.waitForLink(timeout: 15), "link never came up")

        link.send(.list)
        #expect(await log.first(where: { Self.sessions($0) != nil }) != nil,
                "no Sessions frame in reply to List")

        link.send(.create(agent: "echo", name: "smoke", cwd: root.path, cols: 80, rows: 24))
        let created = await log.first {
            Self.sessions($0)?.contains { $0.name == "smoke" && $0.status == .running } ?? false
        }
        let frame = try #require(created, "Create never produced a running session")
        let info = try #require(Self.sessions(frame)?.first { $0.name == "smoke" })
        #expect(info.agent == "echo")
        #expect(info.cwd == root.path)

        link.send(.attach(id: info.id, cols: 80, rows: 24))
        let output = await log.first { frame in
            guard case let .output(id, data, _) = frame, id == info.id else { return false }
            return String(decoding: data, as: UTF8.self).contains("ping-from-pty")
        }
        #expect(output != nil, "PTY bytes never made it back through the codec")

        link.send(.listDir(path: root.path))
        let dirs = await log.first { frame in
            guard case let .dirs(path, names) = frame, path == root.path else { return false }
            return names.contains("state")
        }
        #expect(dirs != nil, "ListDir did not list the scratch dir")

        link.send(.kill(id: info.id))
        let gone = await log.first {
            Self.sessions($0)?.allSatisfy { $0.id != info.id } ?? false
        }
        #expect(gone != nil, "the session outlived Kill")
    }
}
