import Testing

@testable import AgentsHubCore

@Suite struct BellWatchTests {
    private let a = SessionKey(vm: 0, id: "a")

    /// A terminal still draining its attach replay reports zero live bells.
    @Test func zeroNeverFires() {
        var w = BellWatch()
        let rang = w.rang(a, count: 0)
        #expect(rang == false)
    }

    @Test func firesOnARise() {
        var w = BellWatch()
        let rang = w.rang(a, count: 1)
        #expect(rang)
    }

    @Test func theSameCountDoesNotReFire() {
        var w = BellWatch()
        let first = w.rang(a, count: 1)
        let second = w.rang(a, count: 1)
        let third = w.rang(a, count: 1)
        #expect(first)
        #expect(second == false)
        #expect(third == false)
    }

    /// A restart hands over a fresh `TerminalViewState` counting from zero. Treating that
    /// as "no rise" would leave the session silent for the rest of the run.
    @Test func aResetReBaselinesInsteadOfGoingSilent() {
        var w = BellWatch()
        let before = w.rang(a, count: 4)
        let atReset = w.rang(a, count: 0)
        let after = w.rang(a, count: 1)
        #expect(before)
        #expect(atReset == false)
        #expect(after, "still rings after the restart")
    }

    @Test func sessionsAreIndependent() {
        var w = BellWatch()
        let b = SessionKey(vm: 1, id: "a")
        _ = w.rang(a, count: 3)
        let other = w.rang(b, count: 1)
        let mine = w.rang(a, count: 3)
        #expect(other, "same id, different vm")
        #expect(mine == false)
    }

    @Test func aForgottenSessionStartsOver() {
        var w = BellWatch()
        _ = w.rang(a, count: 3)
        w.forget(a)
        let rang = w.rang(a, count: 1)
        #expect(rang)
    }
}
