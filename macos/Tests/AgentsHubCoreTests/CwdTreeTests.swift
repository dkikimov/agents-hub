import Testing

@testable import AgentsHubCore

/// Ported case for case from `src/tui/tree.rs`. `shape` is the cheapest spec of the
/// sidebar that exists, so it comes across too.
@Suite struct CwdTreeTests {
    private let home = "/Users/d"

    private func sess(_ cwd: String) -> SessionInfo {
        SessionInfo(id: cwd, agent: "claude", name: "x", cwd: cwd, status: .stopped, createdAt: 0)
    }

    /// (depth, label) per row — folders by segment, sessions as `#index`.
    private func shape(_ rows: [TreeRow]) -> [String] {
        rows.map { row in
            switch row.node {
            case let .folder(_, seg, _): return "\(row.depth):\(seg)"
            case .elide: return "\(row.depth):…"
            case let .session(i): return "\(row.depth):#\(i)"
            }
        }
    }

    @Test func segmentsNormalisePaths() {
        #expect(segments("/Users/d/Documents/x", home: home) == ["~", "Documents", "x"])
        #expect(segments("/Users/d", home: home) == ["~"])
        #expect(segments("/Users/d/", home: home) == ["~"])
        #expect(segments("~", home: home) == ["~"])
        #expect(segments("~/a/b", home: home) == ["~", "a", "b"])
        #expect(segments("", home: home) == ["~"])
        #expect(segments("/etc/nginx", home: home) == ["/", "etc", "nginx"])
    }

    /// A sibling of $HOME is not $HOME — plain prefix matching gets this wrong, and it
    /// is the one case the Rust test calls out by name.
    @Test func aSiblingOfHomeIsNotHome() {
        #expect(segments("/Users/dx/a", home: home) == ["/", "Users", "dx", "a"])
    }

    @Test func treeGroupsSessionsByCwd() {
        let s = [sess("~/Documents/agents-hub"), sess("~/Documents/notes")]
        #expect(shape(tree(sessions: s, idx: [0, 1], collapsed: [], home: home)) == [
            "0:~", "1:Documents", "2:agents-hub", "3:#0", "2:notes", "3:#1",
        ])
    }

    @Test func collapseElidesTheMiddle() {
        let s = [sess("~/Documents/f1/f2/f3")]
        #expect(shape(tree(sessions: s, idx: [0], collapsed: ["~/Documents"], home: home)) == [
            "0:~", "1:Documents", "2:…", "3:f3", "4:#0",
        ])
    }

    @Test func collapseWithoutAMiddleEmitsNoElide() {
        let s = [sess("~/Documents/a"), sess("~/Documents/b")]
        #expect(shape(tree(sessions: s, idx: [0, 1], collapsed: ["~/Documents"], home: home)) == [
            "0:~", "1:Documents", "2:a", "3:#0", "2:b", "3:#1",
        ])
    }

    @Test func filteredOutSessionsTakeTheirFoldersWithThem() {
        let s = [sess("~/Documents/a"), sess("/etc/nginx")]
        #expect(shape(tree(sessions: s, idx: [1], collapsed: [], home: home)) == [
            "0:/", "1:etc", "2:nginx", "3:#1",
        ])
    }

    /// Swift dictionaries are unordered where tree.rs leaned on BTreeMap, so a missing
    /// `.sorted()` would show up as a sidebar that reshuffles between frames.
    @Test func rowOrderIsStableAcrossRebuilds() {
        let s = (0..<12).map { sess("~/p/d\($0)") }
        let idx = Array(s.indices)
        let first = shape(tree(sessions: s, idx: idx, collapsed: [], home: home))
        for _ in 0..<20 {
            #expect(shape(tree(sessions: s, idx: idx, collapsed: [], home: home)) == first)
        }
    }
}

@Suite struct CwdCompletionTests {
    @Test func splitIsNilUntilASlashIsTyped() {
        #expect(splitCwd("Doc") == nil)
        #expect(splitCwd("~/Doc")?.dir == "~")
        #expect(splitCwd("~/Doc")?.typed == "Doc")
        #expect(splitCwd("/etc/")?.dir == "/etc")
        #expect(splitCwd("/etc/")?.typed == "")
        #expect(splitCwd("/x")?.dir == "/", "a bare leading slash lists the root")
    }

    @Test func dotdirsStayHiddenUntilAskedFor() {
        let names = [".git", ".config", "src", "docs"]
        #expect(filterDirs(names, "") == ["src", "docs"])
        #expect(filterDirs(names, ".") == [".git", ".config"])
        #expect(filterDirs(names, "s") == ["src"])
    }

    @Test func completingEndsOnASlashSoTheMenuDescends() {
        #expect(completeDir("~/Doc", "Documents") == "~/Documents/")
        #expect(completeDir("~/Documents/", "src") == "~/Documents/src/")
    }

    @Test func defaultNameIsTheLastSegment() {
        #expect(defaultName(cwd: "~/Documents/agents-hub", agent: "claude") == "agents-hub")
        #expect(defaultName(cwd: "~/Documents/agents-hub/", agent: "claude") == "agents-hub")
        #expect(defaultName(cwd: "~", agent: "claude") == "claude")
        #expect(defaultName(cwd: "", agent: "codex") == "codex")
    }
}
