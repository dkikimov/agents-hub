import Foundation

/// The wire types, mirroring `src/proto.rs`. Newline-delimited JSON, internally tagged
/// with `"t"`, PTY bytes as standard padded base64.
///
/// Swift's synthesised `Codable` cannot express an internally-tagged enum, so both of
/// these are hand-written. This is the one file where a typo silently loses frames —
/// which is exactly the failure `proto.rs`'s round-trip test exists to prevent, so the
/// same test exists here.

public enum Status: String, Codable, Sendable, Equatable {
    case running = "Running"
    case stopped = "Stopped"
}

public struct SessionInfo: Codable, Sendable, Equatable, Identifiable {
    public let id: String
    public let agent: String
    public let name: String
    public let cwd: String
    public let status: Status
    public let createdAt: UInt64

    enum CodingKeys: String, CodingKey {
        case id, agent, name, cwd, status
        case createdAt = "created_at"
    }

    public init(id: String, agent: String, name: String, cwd: String,
                status: Status, createdAt: UInt64) {
        self.id = id
        self.agent = agent
        self.name = name
        self.cwd = cwd
        self.status = status
        self.createdAt = createdAt
    }
}

public enum Req: Equatable, Sendable {
    case list
    case create(agent: String, name: String, cwd: String, cols: UInt16, rows: UInt16)
    case attach(id: String, cols: UInt16, rows: UInt16)
    case input(id: String, data: Data)
    case resize(id: String, cols: UInt16, rows: UInt16)
    case kill(id: String)
    case restart(id: String, cols: UInt16, rows: UInt16)
    case listDir(path: String)
}

extension Req: Encodable {
    private enum K: String, CodingKey { case t, id, agent, name, cwd, cols, rows, data, path }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case .list:
            try c.encode("List", forKey: .t)
        case let .create(agent, name, cwd, cols, rows):
            try c.encode("Create", forKey: .t)
            try c.encode(agent, forKey: .agent)
            try c.encode(name, forKey: .name)
            try c.encode(cwd, forKey: .cwd)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
        case let .attach(id, cols, rows):
            try c.encode("Attach", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
        case let .input(id, data):
            try c.encode("Input", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(data.base64EncodedString(), forKey: .data)
        case let .resize(id, cols, rows):
            try c.encode("Resize", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
        case let .kill(id):
            try c.encode("Kill", forKey: .t)
            try c.encode(id, forKey: .id)
        case let .restart(id, cols, rows):
            try c.encode("Restart", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(cols, forKey: .cols)
            try c.encode(rows, forKey: .rows)
        case let .listDir(path):
            try c.encode("ListDir", forKey: .t)
            try c.encode(path, forKey: .path)
        }
    }
}

/// Decoding `Req` is only for the round-trip test; the client never receives one.
extension Req: Decodable {
    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .t) {
        case "List":
            self = .list
        case "Create":
            self = .create(agent: try c.decode(String.self, forKey: .agent),
                           name: try c.decode(String.self, forKey: .name),
                           cwd: try c.decode(String.self, forKey: .cwd),
                           cols: try c.decode(UInt16.self, forKey: .cols),
                           rows: try c.decode(UInt16.self, forKey: .rows))
        case "Attach":
            self = .attach(id: try c.decode(String.self, forKey: .id),
                           cols: try c.decode(UInt16.self, forKey: .cols),
                           rows: try c.decode(UInt16.self, forKey: .rows))
        case "Input":
            self = .input(id: try c.decode(String.self, forKey: .id),
                          data: try Self.decodeBase64(c, .data))
        case "Resize":
            self = .resize(id: try c.decode(String.self, forKey: .id),
                           cols: try c.decode(UInt16.self, forKey: .cols),
                           rows: try c.decode(UInt16.self, forKey: .rows))
        case "Kill":
            self = .kill(id: try c.decode(String.self, forKey: .id))
        case "Restart":
            self = .restart(id: try c.decode(String.self, forKey: .id),
                            cols: try c.decode(UInt16.self, forKey: .cols),
                            rows: try c.decode(UInt16.self, forKey: .rows))
        case "ListDir":
            self = .listDir(path: try c.decode(String.self, forKey: .path))
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .t, in: c,
                                                   debugDescription: "unknown Req '\(other)'")
        }
    }

    private static func decodeBase64(_ c: KeyedDecodingContainer<K>, _ key: K) throws -> Data {
        let s = try c.decode(String.self, forKey: key)
        guard let d = Data(base64Encoded: s) else {
            throw DecodingError.dataCorruptedError(forKey: key, in: c,
                                                   debugDescription: "not base64")
        }
        return d
    }
}

public enum Resp: Equatable, Sendable {
    case sessions([SessionInfo])
    case output(id: String, data: Data, live: Bool)
    case exited(id: String, code: Int32)
    case dirs(path: String, names: [String])
    case error(msg: String)
}

extension Resp: Codable {
    private enum K: String, CodingKey { case t, sessions, id, data, live, code, path, names, msg }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        switch try c.decode(String.self, forKey: .t) {
        case "Sessions":
            self = .sessions(try c.decode([SessionInfo].self, forKey: .sessions))
        case "Output":
            let encoded = try c.decode(String.self, forKey: .data)
            // Rust's STANDARD engine emits no whitespace, so the strict decode is
            // correct and fails loudly if that ever stops being true.
            guard let raw = Data(base64Encoded: encoded) else {
                throw DecodingError.dataCorruptedError(forKey: .data, in: c,
                                                       debugDescription: "not base64")
            }
            self = .output(
                id: try c.decode(String.self, forKey: .id),
                data: raw,
                // Mirrors #[serde(default)] on `live`: an older daemon omits the field,
                // and defaulting to false is what stops replay firing side effects.
                live: try c.decodeIfPresent(Bool.self, forKey: .live) ?? false
            )
        case "Exited":
            self = .exited(id: try c.decode(String.self, forKey: .id),
                           code: try c.decode(Int32.self, forKey: .code))
        case "Dirs":
            self = .dirs(path: try c.decode(String.self, forKey: .path),
                         names: try c.decode([String].self, forKey: .names))
        case "Error":
            self = .error(msg: try c.decode(String.self, forKey: .msg))
        case let other:
            throw DecodingError.dataCorruptedError(forKey: .t, in: c,
                                                   debugDescription: "unknown Resp '\(other)'")
        }
    }

    public func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case let .sessions(list):
            try c.encode("Sessions", forKey: .t)
            try c.encode(list, forKey: .sessions)
        case let .output(id, data, live):
            try c.encode("Output", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(data.base64EncodedString(), forKey: .data)
            try c.encode(live, forKey: .live)
        case let .exited(id, code):
            try c.encode("Exited", forKey: .t)
            try c.encode(id, forKey: .id)
            try c.encode(code, forKey: .code)
        case let .dirs(path, names):
            try c.encode("Dirs", forKey: .t)
            try c.encode(path, forKey: .path)
            try c.encode(names, forKey: .names)
        case let .error(msg):
            try c.encode("Error", forKey: .t)
            try c.encode(msg, forKey: .msg)
        }
    }
}
