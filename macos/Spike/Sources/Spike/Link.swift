import Foundation

/// Bytes in, complete NDJSON lines out. The cursor matters: an attach replay arrives as
/// one ~175 KB base64 line, and rescanning from index 0 on every chunk would be
/// quadratic on exactly the frame that matters.
struct LineReader {
    private var buf: [UInt8] = []
    private var scanned = 0

    /// A peer that never sends a newline must not grow the buffer without bound.
    static let maxLine = 8 << 20

    mutating func push(_ chunk: [UInt8], _ onLine: ([UInt8]) -> Void) -> Bool {
        buf.append(contentsOf: chunk)
        var start = 0
        var i = scanned
        while i < buf.count {
            if buf[i] == 0x0A {
                if i > start { onLine(Array(buf[start..<i])) }
                start = i + 1
            }
            i += 1
        }
        if start > 0 { buf.removeFirst(start) }  // one memmove per chunk, not per line
        scanned = buf.count
        return buf.count <= Self.maxLine
    }
}

/// One connection to a daemon. Local and remote differ only in what gets spawned —
/// `agents-hub stdio` already owns the socket, the daemon spawn and the retry poll, so
/// there is no AF_UNIX path here at all.
final class Link {
    private let proc = Process()
    private let outPipe = Pipe()
    private let inPipe = Pipe()
    private var reader = LineReader()
    private var io: DispatchIO?
    private let queue = DispatchQueue(label: "agents-hub.link")

    /// Called on `queue`, never the main actor.
    var onResp: (@Sendable (Resp) -> Void)?

    static var helper: URL {
        let candidates = [
            NSHomeDirectory() + "/.cargo/bin/agents-hub",
            "/usr/local/bin/agents-hub",
            "/opt/homebrew/bin/agents-hub",
        ]
        let found = candidates.first { FileManager.default.isExecutableFile(atPath: $0) }
        return URL(fileURLWithPath: found ?? candidates[0])
    }

    func start(host: String?, remoteBin: String = "agents-hub") throws {
        if let host {
            proc.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
            proc.arguments = ["-A", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=20",
                              host, remoteBin, "stdio"]
        } else {
            proc.executableURL = Self.helper
            proc.arguments = ["stdio"]
        }
        proc.standardOutput = outPipe
        proc.standardInput = inPipe
        // The Rust client nulls this, which makes every remote failure invisible.
        proc.standardError = FileHandle.standardError
        try proc.run()

        let io = DispatchIO(type: .stream,
                            fileDescriptor: outPipe.fileHandleForReading.fileDescriptor,
                            queue: queue,
                            cleanupHandler: { _ in })
        io.setLimit(lowWater: 1)
        io.read(offset: 0, length: Int.max, queue: queue) { [weak self] _, data, _ in
            guard let self, let data, !data.isEmpty else { return }
            let ok = self.reader.push(Array(data)) { line in
                do {
                    self.onResp?(try JSONDecoder().decode(Resp.self, from: Data(line)))
                } catch {
                    log("decode failed: \(error)")
                }
            }
            if !ok { log("peer sent \(LineReader.maxLine) bytes with no newline; dropping") }
        }
        self.io = io
    }

    func send(_ req: Req) {
        guard var line = try? JSONEncoder().encode(req) else { return }
        line.append(0x0A)
        queue.async { [inPipe] in
            try? inPipe.fileHandleForWriting.write(contentsOf: line)
        }
    }

    func stop() {
        io?.close()
        if proc.isRunning { proc.terminate() }
    }
}

func log(_ s: String) {
    FileHandle.standardError.write(Data("spike: \(s)\n".utf8))
}
