import Foundation

/// Sessions grouped into a folder tree by cwd. A port of `src/tui/tree.rs`, kept pure:
/// paths in, rows out, so the whole shape is testable without a window.
///
/// The rows come out pre-flattened rather than as a recursive tree because SwiftUI's
/// `OutlineGroup` cannot express the collapse rule below: collapsing here does not hide
/// a subtree, it *replaces* it with a different one.

public enum TreeNode: Equatable, Sendable {
    case folder(path: String, seg: String, hasSub: Bool)
    /// Stands in for the pass-through folders a collapsed parent dropped.
    case elide(path: String)
    case session(Int)
}

public struct TreeRow: Equatable, Sendable {
    public let depth: Int
    public let node: TreeNode

    public init(depth: Int, node: TreeNode) {
        self.depth = depth
        self.node = node
    }
}

/// A cwd's tree segments, rooted at `~` when it sits under `$HOME`, else at `/`. Applied
/// to remote cwds too: the client cannot know a remote `$HOME`, so a remote `/home/u/x`
/// roots at `/`, which is at least honest. A cwd typed as `~/x` roots correctly on either.
public func segments(_ cwd: String, home: String? = ProcessInfo.processInfo.environment["HOME"]) -> [String] {
    let cwd = trimTrailing(cwd, "/")
    let home = (home?.isEmpty ?? true) ? nil : home

    // A sibling of $HOME is not $HOME: `hasPrefix` alone calls /Users/dx a child of
    // /Users/d, which is the case tree.rs pins a test on.
    let under: String? = home.flatMap { cwd.hasPrefix($0) ? String(cwd.dropFirst($0.count)) : nil }

    let root: String
    let rest: String
    if cwd.isEmpty || cwd == "~" || under == "" {
        (root, rest) = ("~", "")
    } else if cwd.hasPrefix("~/") {
        (root, rest) = ("~", String(cwd.dropFirst(2)))
    } else if let u = under, u.hasPrefix("/") {
        (root, rest) = ("~", String(u.dropFirst()))
    } else {
        (root, rest) = ("/", trimLeading(cwd, "/"))
    }
    return [root] + rest.split(separator: "/").map(String.init)
}

/// Last component of a tree path: `~/a/b` → `b`; the roots `~` and `/` map to themselves.
func segOf(_ path: String) -> String {
    let last = path.split(separator: "/", omittingEmptySubsequences: false).last.map(String.init) ?? ""
    return last.isEmpty ? path : last
}

func joinPath(_ parent: String, _ seg: String) -> String {
    if parent.isEmpty { return seg }
    if parent.hasSuffix("/") { return parent + seg }
    return parent + "/" + seg
}

private func trimTrailing(_ s: String, _ c: Character) -> String {
    var s = s
    while s.last == c { s.removeLast() }
    return s
}

private func trimLeading(_ s: String, _ c: Character) -> String {
    var s = s
    while s.first == c { s.removeFirst() }
    return s
}

/// One VM's sidebar rows. `idx` is the filter-surviving session indices, so a folder with
/// nothing left in it simply never gets built — unless it is a favourite, which is what
/// favouriting is for: the folder outlives its last session.
public func tree(
    sessions: [SessionInfo],
    idx: [Int],
    collapsed: Set<String>,
    favourites: Set<String> = [],
    home: String? = ProcessInfo.processInfo.environment["HOME"]
) -> [TreeRow] {
    var at: [String: [Int]] = [:]
    var kids: [String: Set<String>] = [:]
    var roots: Set<String> = []

    func insert(_ cwd: String) -> String {
        var path = ""
        for (d, seg) in segments(cwd, home: home).enumerated() {
            let parent = path
            path = joinPath(path, seg)
            if d == 0 {
                roots.insert(path)
            } else {
                kids[parent, default: []].insert(path)
            }
            if kids[path] == nil { kids[path] = [] }
        }
        return path
    }

    for i in idx {
        at[insert(sessions[i].cwd), default: []].append(i)
    }
    for path in favourites {
        _ = insert(path)
    }

    // Swift dictionaries are unordered where tree.rs leaned on BTreeMap/BTreeSet, so
    // every traversal below sorts explicitly or the sidebar reshuffles on every frame.
    var out: [TreeRow] = []
    let walker = Walker(at: at, kids: kids, collapsed: collapsed, favourites: favourites)
    for r in roots.sorted() {
        walker.walk(r, 0, &out)
    }
    return out
}

private struct Walker {
    let at: [String: [Int]]
    let kids: [String: Set<String>]
    let collapsed: Set<String>
    let favourites: Set<String>

    func folder(_ path: String, _ hasSub: Bool) -> TreeNode {
        .folder(path: path, seg: segOf(path), hasSub: hasSub)
    }

    func sessionsOf(_ path: String, _ depth: Int, _ out: inout [TreeRow]) {
        for i in at[path] ?? [] {
            out.append(TreeRow(depth: depth, node: .session(i)))
        }
    }

    func walk(_ path: String, _ depth: Int, _ out: inout [TreeRow]) {
        let subs = kids[path]?.count ?? 0
        out.append(TreeRow(depth: depth, node: folder(path, subs > 0)))
        sessionsOf(path, depth + 1, &out)

        if !collapsed.contains(path) {
            for c in (kids[path] ?? []).sorted() {
                walk(c, depth + 1, &out)
            }
            return
        }
        // Collapsed: keep only the folders that actually hold sessions and stand the
        // pass-through ones we dropped up as a single "…".
        var keep: [String] = []
        var elided = false
        descend(path, &keep, &elided)
        if keep.isEmpty { return }
        keep.sort()

        // ponytail: one "…" for the whole subtree, and the survivors show only their last
        // segment — right for the chain this is meant for, lossy on a wide tree. Give each
        // survivor its path relative to the collapsed folder if that ever reads wrong.
        let base: Int
        if elided {
            out.append(TreeRow(depth: depth + 1, node: .elide(path: path)))
            base = depth + 2
        } else {
            base = depth + 1
        }
        for p in keep {
            out.append(TreeRow(depth: base, node: folder(p, false)))
            sessionsOf(p, base + 1, &out)
        }
    }

    /// Every session-bearing descendant of `path`; `elided` records whether anything else
    /// was passed over on the way.
    func descend(_ path: String, _ keep: inout [String], _ elided: inout Bool) {
        for c in (kids[path] ?? []).sorted() {
            if at[c] != nil || favourites.contains(c) { keep.append(c) } else { elided = true }
            descend(c, &keep, &elided)
        }
    }
}
