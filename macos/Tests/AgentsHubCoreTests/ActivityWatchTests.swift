import Foundation
import Testing

@testable import AgentsHubCore

@Suite struct ActivityWatchTests {
    private let a = SessionKey(vm: 0, id: "a")
    private let b = SessionKey(vm: 0, id: "b")
    private let t0 = Date(timeIntervalSince1970: 1_000_000)

    @Test func outputNobodyAskedForIsActivity() {
        var w = ActivityWatch()
        w.output(a, at: t0)
        #expect(w.active(within: 1, now: t0.addingTimeInterval(0.5)) == [a])
    }

    @Test func activityExpiresWithTheWindow() {
        var w = ActivityWatch()
        w.output(a, at: t0)
        #expect(w.active(within: 1, now: t0.addingTimeInterval(1)).isEmpty)
    }

    /// The whole point: a focus report or a SIGWINCH makes the agent repaint, and that
    /// repaint is ours, not its.
    @Test func theRepaintWeProvokedIsNot() {
        var w = ActivityWatch()
        w.poked(a, at: t0)
        w.output(a, at: t0.addingTimeInterval(ActivityWatch.grace))
        #expect(w.active(within: 1, now: t0.addingTimeInterval(0.6)).isEmpty)
    }

    /// An agent that was already working must not go dark because we resized its window.
    @Test func anAgentStillPrintingPastTheGraceLightsUp() {
        var w = ActivityWatch()
        w.poked(a, at: t0)
        w.output(a, at: t0.addingTimeInterval(ActivityWatch.grace + 0.01))
        #expect(w.active(within: 1, now: t0.addingTimeInterval(0.6)) == [a])
    }

    /// A poke at one session says nothing about the next one over.
    @Test func aPokeCoversOnlyItsOwnSession() {
        var w = ActivityWatch()
        w.poked(a, at: t0)
        w.output(a, at: t0)
        w.output(b, at: t0)
        #expect(w.active(within: 1, now: t0) == [b])
    }

    @Test func aForgottenSessionStartsOver() {
        var w = ActivityWatch()
        w.poked(a, at: t0)
        w.forget(a)
        w.output(a, at: t0)
        #expect(w.active(within: 1, now: t0) == [a])
    }
}
