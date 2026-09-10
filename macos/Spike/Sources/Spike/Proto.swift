import Foundation

struct SessionInfo: Decodable, Equatable, Sendable {
    let id: String
    let agent: String
    let name: String
    let cwd: String
    let status: String
    let created_at: UInt64
}

enum Req: Encodable {
    case list
    case attach(id: String, cols: UInt16, rows: UInt16)
    case input(id: String, data: Data)
    case resize(id: String, cols: UInt16, rows: UInt16)

    private enum K: String, CodingKey { case t, id, cols, rows, data }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: K.self)
        switch self {
        case .list:
            try c.encode("List", forKey: .t)
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
        }
    }
}

enum Resp: Sendable {
    case sessions([SessionInfo])
    case output(id: String, data: Data, live: Bool)
    case exited(id: String, code: Int32)
    case dirs(path: String, names: [String])
    case error(String)
}

extension Resp: Decodable {
    private enum K: String, CodingKey { case t, sessions, id, data, live, code, path, names, msg }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: K.self)
        let tag = try c.decode(String.self, forKey: .t)
        switch tag {
        case "Sessions":
            self = .sessions(try c.decode([SessionInfo].self, forKey: .sessions))
        case "Output":
            let encoded = try c.decode(String.self, forKey: .data)
            // Rust's STANDARD engine emits no whitespace, so the strict form is right
            // and fails loudly if that ever stops being true.
            guard let raw = Data(base64Encoded: encoded) else {
                throw DecodingError.dataCorruptedError(forKey: .data, in: c,
                                                       debugDescription: "not base64")
            }
            self = .output(
                id: try c.decode(String.self, forKey: .id),
                data: raw,
                // Mirrors #[serde(default)]: an older daemon omits it, and defaulting
                // false is what stops a replay from firing side effects.
                live: try c.decodeIfPresent(Bool.self, forKey: .live) ?? false
            )
        case "Exited":
            self = .exited(id: try c.decode(String.self, forKey: .id),
                           code: try c.decode(Int32.self, forKey: .code))
        case "Dirs":
            self = .dirs(path: try c.decode(String.self, forKey: .path),
                         names: try c.decode([String].self, forKey: .names))
        case "Error":
            self = .error(try c.decode(String.self, forKey: .msg))
        default:
            throw DecodingError.dataCorruptedError(forKey: .t, in: c,
                                                   debugDescription: "unknown frame '\(tag)'")
        }
    }
}
