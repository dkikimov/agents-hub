import Foundation

/// The pure half of the new-session cwd field, ported from `src/tui/app.rs`. The listing
/// itself comes from `Req.listDir` against the VM that will *host* the session, so a
/// remote path completes against the remote box.

/// A cwd being typed, split into the directory to list and the segment to filter by.
/// `nil` until a `/` has been typed, since before that there is no parent to list.
public func splitCwd(_ cwd: String) -> (dir: String, typed: String)? {
    guard let slash = cwd.lastIndex(of: "/") else { return nil }
    let dir = String(cwd[cwd.startIndex..<slash])
    let typed = String(cwd[cwd.index(after: slash)...])
    return (dir.isEmpty ? "/" : dir, typed)
}

/// A directory listing narrowed to what has been typed. Dotdirs stay hidden until the
/// segment asks for them.
public func filterDirs(_ names: [String], _ typed: String) -> [String] {
    names.filter { $0.hasPrefix(typed) && (typed.hasPrefix(".") || !$0.hasPrefix(".")) }
}

/// Swaps the half-typed last segment of `cwd` for `name`, ending on `/` so the menu then
/// offers that directory's own children.
public func completeDir(_ cwd: String, _ name: String) -> String {
    let head: String
    if let slash = cwd.lastIndex(of: "/") {
        head = String(cwd[cwd.startIndex...slash])
    } else {
        head = ""
    }
    return head + name + "/"
}

/// "~/Documents/agents-hub" → "agents-hub", so a session gets a useful name for free.
public func defaultName(cwd: String, agent: String) -> String {
    var trimmed = cwd
    while trimmed.last == "/" { trimmed.removeLast() }
    let last = trimmed.split(separator: "/").last.map(String.init) ?? ""
    return (last.isEmpty || last == "~") ? agent : last
}
