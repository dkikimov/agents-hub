import Foundation

public enum LinkEvent: Sendable {
    case up
    case down(String?)
    case frame(Resp)
}

/// One reconnecting connection per `[[vm]]`, the twin of `src/tui/link.rs`.
///
/// Local and remote differ only in what gets spawned. `agents-hub stdio` already owns the
/// Unix socket, the daemon auto-spawn and the retry poll, so routing local VMs through it
/// too deletes the whole AF_UNIX path from here.
///
/// ponytail: local VMs pay an extra `agents-hub stdio` hop and one memcpy per byte rather
/// than hitting the socket directly. Swap in socket(2)+connect(2) on the same DispatchIO
/// if a profiler ever cares — nothing above the framing changes.
public final class VMLink: @unchecked Sendable {
    public let index: Int
    public let vm: VMConfig

    private let helper: URL
    private let queue: DispatchQueue
    private let lock = NSLock()

    private var proc: Process?
    private var io: DispatchIO?
    private var errIO: DispatchIO?
    private var inPipe: Pipe?
    private var reader = LineReader()
    private var backoff: UInt64 = 1
    private var stopping = false
    private var connected = false

    /// Delivered on the link's own queue, never the main actor.
    public var onEvent: (@Sendable (Int, LinkEvent) -> Void)?

    /// `extraEnv` exists so a test can point `stdio` at a scratch `AGENTS_HUB_DIR`
    /// without mutating the process environment out from under everything else.
    public init(index: Int, vm: VMConfig, helper: URL = Helper.url,
                extraEnv: [String: String] = [:]) {
        self.index = index
        self.vm = vm
        self.helper = helper
        self.extraEnv = extraEnv
        self.queue = DispatchQueue(label: "agents-hub.link.\(index)")
    }

    private let extraEnv: [String: String]

    public func start() {
        queue.async { [self] in connect() }
    }

    public func stop() {
        queue.async { [self] in
            stopping = true
            teardown(reason: nil, notify: false)
        }
    }

    public func send(_ req: Req) {
        queue.async { [self] in
            guard connected, let pipe = inPipe else { return }  // dropped while down
            guard var line = try? JSONEncoder().encode(req) else { return }
            line.append(0x0A)
            do {
                try pipe.fileHandleForWriting.write(contentsOf: line)
            } catch {
                teardown(reason: "write failed: \(error.localizedDescription)", notify: true)
            }
        }
    }

    // MARK: - private, all on `queue`

    private func connect() {
        guard !stopping else { return }
        let p = Process()
        let out = Pipe()
        let inp = Pipe()
        let errPipe = Pipe()

        if let host = vm.ssh {
            p.executableURL = URL(fileURLWithPath: "/usr/bin/ssh")
            // -A so the remote `stdio` has an $SSH_AUTH_SOCK to point agent.upstream at
            // without depending on ForwardAgent in ~/.ssh/config. BatchMode so a password
            // prompt can never hang the app.
            p.arguments = ["-A", "-o", "BatchMode=yes", "-o", "ServerAliveInterval=20",
                           host, vm.remoteBin, "stdio"]
        } else {
            guard FileManager.default.isExecutableFile(atPath: helper.path) else {
                fail("agents-hub not found at \(helper.path)")
                return
            }
            p.executableURL = helper
            p.arguments = ["stdio"]
        }
        p.standardOutput = out
        p.standardInput = inp
        // The Rust client nulls this, which makes every remote failure invisible.
        p.standardError = errPipe
        if !extraEnv.isEmpty {
            // Setting `environment` replaces it wholesale, so merge rather than assign.
            p.environment = ProcessInfo.processInfo.environment.merging(extraEnv) { _, new in new }
        }

        do {
            try p.run()
        } catch {
            fail("could not start: \(error.localizedDescription)")
            return
        }

        proc = p
        inPipe = inp
        reader.reset()
        connected = true
        backoff = 1
        emit(.up)

        drainStderr(errPipe)

        let io = DispatchIO(type: .stream,
                            fileDescriptor: out.fileHandleForReading.fileDescriptor,
                            queue: queue,
                            cleanupHandler: { _ in })
        io.setLimit(lowWater: 1)
        io.read(offset: 0, length: Int.max, queue: queue) { [weak self] done, data, _ in
            guard let self else { return }
            if let data, !data.isEmpty {
                let ok = self.reader.push(Array(data)) { line in
                    do {
                        self.emit(.frame(try JSONDecoder().decode(Resp.self, from: Data(line))))
                    } catch {
                        NSLog("agents-hub: undecodable frame from %@: %@",
                              self.vm.name, String(describing: error))
                    }
                }
                if !ok {
                    self.teardown(reason: "peer sent no newline in \(LineReader.maxLine) bytes",
                                  notify: true)
                    return
                }
            }
            if done {
                self.teardown(reason: self.connected ? "link closed" : nil, notify: true)
            }
        }
        self.io = io
    }

    /// ssh writes its diagnostics here and then usually exits; surfacing it is the
    /// difference between "offline" and "Permission denied (publickey)".
    ///
    /// Same `DispatchIO` as stdout rather than a thread blocked in `read`: that thread
    /// parked for the life of the process, one per VM, to carry a line of text that
    /// arrives once per connection.
    private func drainStderr(_ pipe: Pipe) {
        let io = DispatchIO(type: .stream,
                            fileDescriptor: pipe.fileHandleForReading.fileDescriptor,
                            queue: queue,
                            cleanupHandler: { _ in })
        io.setLimit(lowWater: 1)
        io.read(offset: 0, length: Int.max, queue: queue) { [weak self] _, data, _ in
            guard let self, let data, !data.isEmpty else { return }
            let text = String(decoding: data, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            guard !text.isEmpty else { return }
            lastStderr = text
            NSLog("agents-hub: %@: %@", vm.name, text)
        }
        errIO = io
    }

    private var lastStderr: String?

    private func fail(_ reason: String) {
        connected = false
        emit(.down(reason))
        scheduleRetry()
    }

    private func teardown(reason: String?, notify: Bool) {
        // Both channels go before `proc`: the pipes die with the Process, and their fds
        // with them, so a reader still holding one would be reading a reused descriptor.
        io?.close(flags: .stop)
        io = nil
        errIO?.close(flags: .stop)
        errIO = nil
        inPipe = nil
        if let p = proc, p.isRunning { p.terminate() }
        proc = nil
        let wasConnected = connected
        connected = false
        if notify && wasConnected {
            emit(.down(lastStderr ?? reason))
            lastStderr = nil
        }
        if notify { scheduleRetry() }
    }

    /// A VM being down is a display state, never a reason to give up. 1s doubling to 30s,
    /// reset on a successful link.
    private func scheduleRetry() {
        guard !stopping else { return }
        let delay = backoff
        backoff = min(backoff * 2, 30)
        queue.asyncAfter(deadline: .now() + .seconds(Int(delay))) { [weak self] in
            self?.connect()
        }
    }

    private func emit(_ e: LinkEvent) {
        onEvent?(index, e)
    }
}
