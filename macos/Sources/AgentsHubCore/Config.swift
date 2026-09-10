import Foundation

public struct VMConfig: Decodable, Sendable, Equatable {
    public let name: String
    /// SSH host alias from ~/.ssh/config. Absent = this machine.
    public let ssh: String?
    public let remoteBin: String

    enum CodingKeys: String, CodingKey {
        case name, ssh
        case remoteBin = "remote_bin"
    }

    public init(name: String, ssh: String? = nil, remoteBin: String = "agents-hub") {
        self.name = name
        self.ssh = ssh
        self.remoteBin = remoteBin
    }

    public var isLocal: Bool { ssh == nil }
}

public struct AgentConfig: Decodable, Sendable, Equatable {
    public let command: [String]
    public let resume: [String]?
}

public struct HubConfig: Decodable, Sendable, Equatable {
    public let vm: [VMConfig]
    public let agents: [String: AgentConfig]

    /// Sorted, matching the `BTreeMap` order the Rust picker uses.
    public var agentNames: [String] { agents.keys.sorted() }

    public static let empty = HubConfig(vm: [], agents: [:])

    public init(vm: [VMConfig], agents: [String: AgentConfig]) {
        self.vm = vm
        self.agents = agents
    }
}

public enum ConfigError: LocalizedError {
    case helperMissing(String)
    case helperFailed(String)

    public var errorDescription: String? {
        switch self {
        case let .helperMissing(p): return "agents-hub not found (looked at \(p))"
        case let .helperFailed(m): return "agents-hub config --json failed: \(m)"
        }
    }
}

/// Reads the config by asking the Rust binary for it as JSON, rather than parsing TOML
/// here. One parser stays authoritative, the client can never disagree with the daemon
/// about what a VM is, and no TOML dependency joins a project whose stated convention is
/// "no clap, no dirs".
public func loadConfig(helper: URL = Helper.url) throws -> HubConfig {
    guard FileManager.default.isExecutableFile(atPath: helper.path) else {
        throw ConfigError.helperMissing(helper.path)
    }
    let p = Process()
    p.executableURL = helper
    p.arguments = ["config", "--json"]
    let out = Pipe()
    let err = Pipe()
    p.standardOutput = out
    p.standardError = err
    try p.run()
    let data = out.fileHandleForReading.readDataToEndOfFile()
    let errText = String(decoding: err.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    p.waitUntilExit()
    guard p.terminationStatus == 0 else {
        throw ConfigError.helperFailed(errText.isEmpty ? "exit \(p.terminationStatus)" : errText)
    }
    return try JSONDecoder().decode(HubConfig.self, from: data)
}

public enum Helper {
    /// A GUI app launched from Finder gets the minimal launchd PATH, so `agents-hub`
    /// living in ~/.cargo/bin would simply not be found. Look in the bundle first — a
    /// bundled copy also makes client/daemon version skew impossible.
    public static let url: URL = {
        if let bundled = Bundle.main.url(forAuxiliaryExecutable: "agents-hub") {
            return bundled
        }
        let candidates = [
            NSHomeDirectory() + "/.cargo/bin/agents-hub",
            "/usr/local/bin/agents-hub",
            "/opt/homebrew/bin/agents-hub",
        ]
        let found = candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
        return URL(fileURLWithPath: found ?? candidates[0])
    }()
}
