import Foundation
import Testing

@testable import AgentsHubCore

/// The twin of `proto.rs::frames_round_trip`. A variant that silently fails to encode
/// disappears at runtime instead of failing loudly, which is the bug that test exists for.
@Suite struct ProtoTests {
    private func line<T: Encodable>(_ v: T) throws -> String {
        String(decoding: try JSONEncoder().encode(v), as: UTF8.self)
    }

    @Test func everyRequestRoundTrips() throws {
        let reqs: [Req] = [
            .list,
            .create(agent: "claude", name: "api", cwd: "/tmp", cols: 80, rows: 24),
            .attach(id: "x", cols: 80, rows: 24),
            .input(id: "x", data: Data([0x68, 0x69, 0x1b, 0x5b, 0x30, 0x6d])),
            .resize(id: "x", cols: 100, rows: 30),
            .kill(id: "x"),
            .restart(id: "x", cols: 80, rows: 24),
            .listDir(path: "~"),
        ]
        for r in reqs {
            let encoded = try line(r)
            #expect(!encoded.contains("\n"), "frames must be single-line")
            #expect(try JSONDecoder().decode(Req.self, from: Data(encoded.utf8)) == r)
        }
    }

    @Test func everyResponseRoundTrips() throws {
        let resps: [Resp] = [
            .sessions([SessionInfo(id: "1", agent: "claude", name: "api", cwd: "/tmp",
                                   status: .stopped, createdAt: 7)]),
            .sessions([]),
            .output(id: "x", data: Data([0, 255, 10, 13]), live: true),
            .exited(id: "x", code: -1),
            .dirs(path: "~", names: ["src"]),
            .dirs(path: "/", names: []),
            .error(msg: "boom"),
        ]
        for r in resps {
            let encoded = try line(r)
            #expect(!encoded.contains("\n"))
            #expect(try JSONDecoder().decode(Resp.self, from: Data(encoded.utf8)) == r)
        }
    }

    /// Mirrors `#[serde(default)]` on `live`: an older daemon omits the field, and a new
    /// client must not replay side effects because of it.
    @Test func outputWithoutLiveDefaultsToReplay() throws {
        let raw = #"{"t":"Output","id":"x","data":"aGk="}"#
        let decoded = try JSONDecoder().decode(Resp.self, from: Data(raw.utf8))
        #expect(decoded == .output(id: "x", data: Data("hi".utf8), live: false))
    }

    @Test func base64IsByteExact() throws {
        let raw = Data((0...255).map { UInt8($0) })
        let frame: Resp = .output(id: "x", data: raw, live: true)
        let back = try JSONDecoder().decode(Resp.self, from: try JSONEncoder().encode(frame))
        guard case let .output(_, data, _) = back else { Issue.record("wrong variant"); return }
        #expect(data == raw)
    }

    /// The exact bytes the Rust side emits, so a field rename on either side fails here
    /// rather than as a silently dropped frame at runtime.
    @Test func wireShapeMatchesRust() throws {
        #expect(try line(Req.list) == #"{"t":"List"}"#)

        let info = SessionInfo(id: "a", agent: "claude", name: "n", cwd: "/tmp",
                               status: .running, createdAt: 3)
        let json = try JSONSerialization.jsonObject(
            with: try JSONEncoder().encode(info)) as? [String: Any]
        #expect(json?["created_at"] as? UInt64 == 3, "serde uses the snake_case field name")
        #expect(json?["status"] as? String == "Running", "serde emits the bare variant name")
    }

    @Test func unknownFramesFailLoudlyRatherThanSilently() {
        let raw = #"{"t":"SomethingNewer","id":"x"}"#
        #expect(throws: (any Error).self) {
            try JSONDecoder().decode(Resp.self, from: Data(raw.utf8))
        }
    }
}
