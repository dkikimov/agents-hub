import Testing

@testable import AgentsHubCore

@Suite struct AgentWatchTests {
    private let a = SessionKey(vm: 0, id: "a")
    private let b = SessionKey(vm: 0, id: "b")

    /// A reconnect reads every session for the first time; none of that is news.
    @Test func firstSightIsNotNews() {
        var w = AgentWatch()
        #expect(w.update(a, .idle, watching: false) == nil)
        #expect(w.update(b, .blocked, watching: false) == nil)
        #expect(w.done.isEmpty)
    }

    @Test func aTurnThatEndsOffscreenIsDoneUntilSeen() {
        var w = AgentWatch()
        _ = w.update(a, .working, watching: false)
        #expect(w.update(a, .idle, watching: false) == .finished)
        #expect(w.done == [a])
        w.seen(a)
        #expect(w.done.isEmpty)
    }

    @Test func theSessionInFrontOfYouNeverGoesDoneOrNotifies() {
        var w = AgentWatch()
        _ = w.update(a, .working, watching: true)
        #expect(w.update(a, .blocked, watching: true) == nil)
        #expect(w.update(a, .idle, watching: true) == nil)
        #expect(w.done.isEmpty)
    }

    @Test func aPromptAsksOnceAndAnAnswerOffscreenIsDone() {
        var w = AgentWatch()
        _ = w.update(a, .working, watching: false)
        #expect(w.update(a, .blocked, watching: false) == .needsYou)
        #expect(w.update(a, .blocked, watching: false) == nil, "the same prompt, again")
        #expect(w.update(a, .idle, watching: false) == .finished)
    }

    /// Unknown → idle is a launch settling or a restart, not a turn ending.
    @Test func onlyWorkOrAPromptEndsInDone() {
        var w = AgentWatch()
        _ = w.update(a, .unknown, watching: false)
        #expect(w.update(a, .idle, watching: false) == nil)
        #expect(w.done.isEmpty)
    }

    @Test func newWorkOrAStopClearsDone() {
        var w = AgentWatch()
        _ = w.update(a, .working, watching: false)
        _ = w.update(a, .idle, watching: false)
        _ = w.update(a, .working, watching: false)
        #expect(w.done.isEmpty)
        _ = w.update(a, .idle, watching: false)
        _ = w.update(a, .unknown, watching: false)
        #expect(w.done.isEmpty)
    }

    @Test func aForgottenSessionStartsOver() {
        var w = AgentWatch()
        _ = w.update(a, .working, watching: false)
        _ = w.update(a, .idle, watching: false)
        w.forget(a)
        #expect(w.done.isEmpty)
        #expect(w.update(a, .idle, watching: false) == nil)
    }
}
