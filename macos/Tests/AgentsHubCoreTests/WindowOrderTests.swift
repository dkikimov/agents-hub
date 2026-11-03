import Testing

@testable import AgentsHubCore

@Suite struct WindowOrderTests {
    @Test func theFirstWindowOwnsTheSessionsOnArrival() {
        var o = WindowOrder<String>()
        #expect(o.owns("a"))
        o.opened("a")
        #expect(o.owner == "a")
        #expect(o.owns("a"))
    }

    /// Opening a second window must not resize anything by itself; using it does.
    @Test func aNewWindowWaitsUntilItIsUsed() {
        var o = WindowOrder<String>()
        o.opened("a")
        o.opened("b")
        #expect(o.owner == "a")
        #expect(!o.owns("b"))
        #expect(o.used("b"))
        #expect(o.owner == "b")
        #expect(!o.owns("a"))
    }

    /// Focus moving inside the window in use, or a keystroke in it, is not a handover.
    @Test func usingTheOwnerAgainChangesNothing() {
        var o = WindowOrder<String>()
        o.opened("a")
        o.opened("b")
        #expect(!o.used("a"))
        #expect(o.used("b"))
        #expect(!o.used("b"))
    }

    /// The key notice can land before the window has joined; joining must not demote it.
    @Test func usedBeforeOpenedStaysTheOwner() {
        var o = WindowOrder<String>()
        o.opened("a")
        #expect(o.used("b"))
        o.opened("b")
        #expect(o.owner == "b")
    }

    @Test func closingTheOwnerHandsOverToTheOneUsedBeforeIt() {
        var o = WindowOrder<String>()
        o.opened("a")
        o.opened("b")
        o.opened("c")
        _ = o.used("b")
        _ = o.used("c")
        #expect(o.closed("c") == "b")
        #expect(o.owner == "b")
    }

    @Test func closingAnotherWindowIsNoHandover() {
        var o = WindowOrder<String>()
        o.opened("a")
        o.opened("b")
        _ = o.used("b")
        #expect(o.closed("a") == nil)
        #expect(o.owner == "b")
    }

    @Test func closingTheLastWindowLeavesNobodyToDisagree() {
        var o = WindowOrder<String>()
        o.opened("a")
        #expect(o.closed("a") == nil)
        #expect(o.owner == nil)
        #expect(o.owns("z"))
    }
}
