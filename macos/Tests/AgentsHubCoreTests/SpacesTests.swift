import Foundation
import Testing

@testable import AgentsHubCore

@Suite struct SpacesTests {
    private let a = MemberKey(vm: "local", id: "a")
    private let b = MemberKey(vm: "local", id: "b")
    private let remote = MemberKey(vm: "sas", id: "a")

    @Test func aSessionIsInAtMostOneSpace() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        let y = book.create(name: "y")
        book.move([a, b], to: x)
        book.move([a], to: y)
        #expect(book.space(of: a) == y)
        #expect(book.space(of: b) == x)
        book.move([b], to: nil)
        #expect(book.space(of: b) == nil)
        #expect(book.contains(.unsorted, b))
        #expect(!book.contains(.unsorted, a))
        #expect(book.contains(.all, a))
    }

    /// The same id on two VMs is two sessions.
    @Test func membersAreKeyedByVMName() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        book.move([remote], to: x)
        #expect(book.contains(.space(x), remote))
        #expect(!book.contains(.space(x), a))
    }

    @Test func pruneTouchesOnlyTheVMThatReported() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        book.move([a, b, remote], to: x)
        book.prune(vm: "local", alive: ["b"])
        #expect(book.space(x)?.members == [b, remote])
    }

    @Test func deletingASpaceLeavesItsSessionsUnsorted() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        book.move([a], to: x)
        book.delete(x)
        #expect(book.spaces.isEmpty)
        #expect(book.contains(.unsorted, a))
        #expect(!book.contains(.space(x), a))
    }

    @Test func renameKeepsOrderAndMembers() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        let y = book.create(name: "y")
        book.move([a], to: x)
        book.rename(x, to: "z")
        #expect(book.spaces.map(\.name) == ["z", "y"])
        #expect(book.spaces.map(\.id) == [x, y])
        #expect(book.space(of: a) == x)
    }

    @Test func bookAndViewSurviveEncoding() throws {
        var book = SpaceBook()
        let x = book.create(name: "x")
        book.move([a, remote], to: x)
        let decoded = try JSONDecoder().decode(SpaceBook.self, from: JSONEncoder().encode(book))
        #expect(decoded == book)
        for view in [SpaceView.space(x), .unsorted, .all] {
            let back = try JSONDecoder().decode(SpaceView.self, from: JSONEncoder().encode(view))
            #expect(back == view)
        }
    }

    @Test func moodReadsLikeTheStatusDot() {
        #expect(Mood(online: false, status: .running, activity: .blocked, done: false) == .offline)
        #expect(Mood(online: true, status: .stopped, activity: .working, done: true) == .stopped)
        #expect(Mood(online: true, status: .running, activity: .blocked, done: true) == .blocked)
        #expect(Mood(online: true, status: .running, activity: .working, done: false) == .working)
        #expect(Mood(online: true, status: .running, activity: .unknown, done: true) == .done)
        #expect(Mood(online: true, status: .running, activity: .idle, done: false) == .idle)
    }

    /// herdr's order: an unseen finish is more urgent than a turn still in progress.
    @Test func rollupTakesTheMostUrgent() {
        #expect(Mood.rollup([Mood]()) == .empty)
        #expect(Mood.rollup([.idle, .working, .stopped]) == .working)
        #expect(Mood.rollup([.working, .done]) == .done)
        #expect(Mood.rollup([.done, .blocked, .offline]) == .blocked)
    }

    // MARK: - the space bar

    /// Before the first space the bar is only All, so nothing changes for someone who
    /// never makes one — not even an Unsorted chip holding every session.
    @Test func noSpacesMeansJustAll() {
        let book = SpaceBook()
        #expect(book.bar(showing: .all, unsorted: 5) == [.all])
        #expect(book.landing(unsorted: 5) == .all)
    }

    @Test func unsortedShowsWhileItHoldsSomethingOrIsShowing() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        let y = book.create(name: "y")
        #expect(book.bar(showing: .all, unsorted: 2) == [.space(x), .space(y), .unsorted, .all])
        #expect(book.bar(showing: .all, unsorted: 0) == [.space(x), .space(y), .all])
        // Emptied while you look at it, it stays rather than vanishing under the cursor.
        #expect(book.bar(showing: .unsorted, unsorted: 0) == [.space(x), .space(y), .unsorted, .all])
    }

    @Test func steppingWrapsBothWays() {
        let x = UUID(), y = UUID()
        let bar: [SpaceView] = [.space(x), .space(y), .all]
        #expect(SpaceView.space(x).stepped(by: 1, in: bar) == .space(y))
        #expect(SpaceView.all.stepped(by: 1, in: bar) == .space(x))
        #expect(SpaceView.space(x).stepped(by: -1, in: bar) == .all)
        #expect(SpaceView.space(y).stepped(by: -4, in: bar) == .space(x))
        #expect(SpaceView.all.stepped(by: 1, in: [.all]) == .all)
        // A view the bar no longer has steps from the first chip.
        #expect(SpaceView.unsorted.stepped(by: 1, in: bar) == .space(y))
    }

    @Test func deletingTheSpaceShowingLandsWhereItsSessionsWent() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        _ = book.create(name: "y")
        book.delete(x)
        #expect(book.landing(unsorted: 3) == .unsorted)
        #expect(book.landing(unsorted: 0) == .all)
    }

    @Test func aDeletedSpaceIsNoLongerAView() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        #expect(book.has(.space(x)))
        book.delete(x)
        #expect(!book.has(.space(x)))
        #expect(book.has(.unsorted) && book.has(.all))
    }

    @Test func namingASpaceTwiceReusesIt() {
        var book = SpaceBook()
        let x = book.space(named: "hitrobot")
        #expect(book.space(named: "hitrobot") == x)
        #expect(book.space(named: "infra") != x)
        #expect(book.spaces.map(\.name) == ["hitrobot", "infra"])
    }

    // MARK: - moving

    @Test func moveTargetsSkipWhereTheSessionsAlreadyAre() {
        var book = SpaceBook()
        let x = book.create(name: "x")
        let y = book.create(name: "y")
        #expect(book.targets(from: [x]) == [y, nil])
        #expect(book.targets(from: [nil]) == [x, y])
        // Mixed homes: every space is somewhere at least one of them is not.
        #expect(book.targets(from: [x, nil]) == [x, y, nil])
        #expect(book.targets(from: [x, y]) == [x, y, nil])
    }

    // MARK: - sessions made inside a space

    private func info(_ id: String, _ agent: String, _ name: String,
                      parent: String? = nil) -> SessionInfo {
        SessionInfo(id: id, agent: agent, name: name, cwd: "~", status: .running,
                    createdAt: 0, parent: parent)
    }

    @Test func aNewSessionIsClaimedByVMAgentAndName() {
        var pending = PendingCreates()
        let x = UUID()
        pending.expect(vm: 0, agent: "claude", name: "api", space: x)
        #expect(pending.claim(vm: 1, [info("1", "claude", "api")]).isEmpty, "another VM")
        #expect(pending.claim(vm: 0, [info("2", "codex", "api")]).isEmpty, "another agent")
        #expect(pending.claim(vm: 0, [info("3", "claude", "web")]).isEmpty, "another name")
        let claimed = pending.claim(vm: 0, [info("4", "claude", "api")])
        #expect(claimed.map(\.id) == ["4"] && claimed.map(\.space) == [x])
        #expect(pending.claim(vm: 0, [info("5", "claude", "api")]).isEmpty, "claimed once")
    }

    /// A companion shell inherits its parent's name, so it must never take the slot.
    @Test func aShellIsNeverClaimed() {
        var pending = PendingCreates()
        pending.expect(vm: 0, agent: "shell", name: "api", space: UUID())
        #expect(pending.claim(vm: 0, [info("s", "shell", "api", parent: "p")]).isEmpty)
        #expect(pending.claim(vm: 0, [info("t", "shell", "api")]).count == 1)
    }

    @Test func twoCreatesOfTheSameNameGoOldestFirst() {
        var pending = PendingCreates()
        let x = UUID(), y = UUID()
        pending.expect(vm: 0, agent: "claude", name: "api", space: x)
        pending.expect(vm: 0, agent: "claude", name: "api", space: y)
        let claimed = pending.claim(vm: 0, [info("1", "claude", "api"), info("2", "claude", "api")])
        #expect(claimed.map(\.id) == ["1", "2"])
        #expect(claimed.map(\.space) == [x, y])
    }

    /// The whole round, as `AppModel` runs it: create in a space, claim the id off the
    /// next frame, and lose it from the space once a later frame no longer lists it.
    @Test func createClaimKillRoundTrip() {
        var book = SpaceBook()
        var pending = PendingCreates()
        let x = book.create(name: "x")
        pending.expect(vm: 0, agent: "claude", name: "api", space: x)
        for (id, space) in pending.claim(vm: 0, [info("9", "claude", "api")]) {
            book.move([MemberKey(vm: "local", id: id)], to: space)
        }
        #expect(book.contains(.space(x), MemberKey(vm: "local", id: "9")))
        book.prune(vm: "local", alive: [])
        #expect(book.space(x)?.members.isEmpty == true)
        #expect(book.has(.space(x)), "an emptied space stays, ready for the next session")
    }
}
