import Foundation
import Testing

@testable import AgentsHubCore

@Suite struct AttachFeedTests {
    private final class Probe: @unchecked Sendable {
        var parsed = Data()
        var exits: [UInt32] = []
        var sized = 0
        var settled = 0
    }

    private let fallback = AttachFeed.Grid(cols: 49, rows: 17)
    private let real = AttachFeed.Grid(cols: 125, rows: 44)

    private func bound() -> (AttachFeed, Probe) {
        let feed = AttachFeed()
        let probe = Probe()
        feed.bind(
            parse: { probe.parsed.append($0) },
            exit: { probe.exits.append($0) },
            sized: { probe.sized += 1 },
            settled: { probe.settled += 1 }
        )
        return (feed, probe)
    }

    /// The fresh surface's default grid is what garbled a stopped agent's history.
    @Test func replayWaitsForTheLaidOutGrid() {
        let (feed, probe) = bound()
        feed.receive(Data("history".utf8))
        let early = feed.resized(fallback)
        let laidOutFirst = feed.laidOut(real)
        #expect(early == false, "the default grid never reaches the PTY")
        #expect(laidOutFirst == false)
        #expect(probe.sized == 0)

        let matched = feed.resized(real)
        #expect(matched)
        #expect(probe.sized == 1)
        #expect(probe.parsed.isEmpty, "ghostty has not applied the size yet")

        feed.release()
        #expect(probe.parsed == Data("history".utf8))
        #expect(probe.settled == 1)
    }

    /// Ghostty can report the grid before the view state publishes it; the resize it
    /// withheld then has to come back out of `laidOut`, or the PTY keeps its old size.
    @Test func laidOutAfterTheReportAsksForTheWithheldResize() {
        let (feed, probe) = bound()
        let withheld = feed.resized(real)
        let matched = feed.laidOut(real)
        let again = feed.laidOut(real)
        #expect(withheld == false)
        #expect(matched)
        #expect(again == false, "sent exactly once")
        #expect(probe.sized == 1)
    }

    @Test func onceSizedBytesAndResizesFlowStraightThrough() {
        let (feed, probe) = bound()
        _ = feed.laidOut(real)
        _ = feed.resized(real)
        feed.receive(Data("a".utf8))
        feed.release()
        feed.receive(Data("b".utf8))
        let drag = feed.resized(AttachFeed.Grid(cols: 100, rows: 30))
        let settle = feed.laidOut(AttachFeed.Grid(cols: 100, rows: 30))
        #expect(probe.parsed == Data("ab".utf8))
        #expect(drag)
        #expect(settle == false, "a drag is ghostty's to report, not ours")
        #expect(probe.sized == 1)
    }

    /// Settling needs the replay too: a surface that is ready before a slow link answers
    /// must not open writeback onto history it has not parsed yet.
    @Test func settlesOnlyOnceTheReplayHasArrived() {
        let (feed, probe) = bound()
        _ = feed.laidOut(real)
        _ = feed.resized(real)
        feed.release()
        #expect(probe.settled == 0)
        feed.receive(Data())
        feed.receive(Data("live".utf8))
        #expect(probe.settled == 1)
    }

    @Test func anExitWhileHeldLandsAfterItsBytes() {
        let (feed, probe) = bound()
        feed.receive(Data("bye".utf8))
        feed.finish(exitCode: 3)
        #expect(probe.exits.isEmpty)
        feed.release()
        #expect(probe.parsed == Data("bye".utf8))
        #expect(probe.exits == [3])
        #expect(probe.settled == 1)
    }

    @Test func writebackIsDroppedUntilOpened() {
        let feed = AttachFeed()
        let early = feed.allowsWriteback(Data("\u{1b}[?62;22;52c".utf8))
        let dropped = feed.openWriteback()
        let late = feed.allowsWriteback(Data("x".utf8))
        #expect(early == false)
        #expect(dropped == 12)
        #expect(late)
    }

    @Test func anUnviewedSessionKeepsOnlyTheNewestBacklog() {
        let (feed, probe) = bound()
        feed.receive(Data(repeating: 0x61, count: AttachFeed.backlogLimit))
        feed.receive(Data("tail".utf8))
        feed.release()
        #expect(probe.parsed.count == AttachFeed.backlogLimit)
        #expect(probe.parsed.suffix(4) == Data("tail".utf8))
    }
}
