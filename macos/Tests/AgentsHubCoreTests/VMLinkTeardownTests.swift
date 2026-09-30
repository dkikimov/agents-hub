import Foundation
import Testing

@testable import AgentsHubCore

@Suite struct VMLinkTeardownTests {
    /// A link that dies straight away hits teardown while its read sources are still
    /// registered; if a pipe's fd is closed before dispatch lets go of it, libdispatch
    /// traps with EV_VANISHED and takes the process with it.
    @Test func churningLinksNeverCloseAnFdUnderDispatch() async throws {
        let links = (0..<200).map {
            VMLink(index: $0, vm: VMConfig(name: "churn"), helper: URL(fileURLWithPath: "/bin/cat"))
        }
        for link in links { link.start() }
        try await Task.sleep(for: .milliseconds(1500))
        for link in links { link.stop() }
        try await Task.sleep(for: .milliseconds(500))
    }

    /// A link that is down already has a retry on its backoff; dialling again on top of
    /// it would start a second retry chain.
    @Test func reconnectLeavesADownLinkToItsRetry() async throws {
        let downs = Counter()
        let link = VMLink(index: 0, vm: VMConfig(name: "down"),
                          helper: URL(fileURLWithPath: "/nonexistent/agents-hub"))
        link.onEvent = { _, event in if case .down = event { downs.bump() } }
        link.start()
        try await Task.sleep(for: .milliseconds(200))
        link.reconnect()
        try await Task.sleep(for: .milliseconds(300))
        link.stop()
        #expect(downs.value == 1, "reconnect dialled a down link outside its backoff")
    }
}

private final class Counter: @unchecked Sendable {
    private let lock = NSLock()
    private var count = 0

    func bump() {
        lock.lock()
        count += 1
        lock.unlock()
    }

    var value: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }
}
