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
}
