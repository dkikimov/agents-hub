//! Sessions grouped into a folder tree by cwd. Pure: paths in, rows out, so the
//! whole shape is testable without a terminal.

use crate::proto::SessionInfo;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// A cwd's tree segments, rooted at `~` when it sits under `$HOME`, else at `/`.
/// Applied to remote cwds too: the client can't know a remote `$HOME`, so a remote
/// `/home/u/x` roots at `/`, which is at least honest. Paths typed as `~/x` in the
/// new-session modal arrive as `~/x` and root correctly on either kind of VM.
pub fn segments(cwd: &str) -> Vec<String> {
    let cwd = cwd.trim_end_matches('/');
    let home = std::env::var("HOME").ok().filter(|h| !h.is_empty());
    let under = home.as_deref().and_then(|h| cwd.strip_prefix(h));
    let (root, rest) = if cwd.is_empty() || cwd == "~" || under == Some("") {
        ("~", "")
    } else if let Some(r) = cwd
        .strip_prefix("~/")
        .or_else(|| under.and_then(|r| r.strip_prefix('/')))
    {
        ("~", r)
    } else {
        ("/", cwd.trim_start_matches('/'))
    };
    std::iter::once(root.to_string())
        .chain(rest.split('/').filter(|s| !s.is_empty()).map(str::to_string))
        .collect()
}

/// Last component of a tree path: `~/a/b` → `b`, and the roots `~` and `/` map to themselves.
fn seg_of(path: &str) -> &str {
    match path.rsplit('/').next() {
        Some(s) if !s.is_empty() => s,
        _ => path,
    }
}

fn join(parent: &str, seg: &str) -> String {
    match parent {
        "" => seg.to_string(),
        p if p.ends_with('/') => format!("{p}{seg}"),
        p => format!("{p}/{seg}"),
    }
}

pub enum Node {
    Folder {
        path: String,
        seg: String,
        has_sub: bool,
    },
    /// Stands in for the pass-through folders a collapsed parent dropped.
    Elide,
    Session(usize),
}

struct Tree<'a> {
    /// Folder path → the sessions whose cwd is exactly that folder.
    at: &'a BTreeMap<String, Vec<usize>>,
    kids: &'a BTreeMap<String, BTreeSet<String>>,
    collapsed: &'a HashSet<String>,
}

impl Tree<'_> {
    fn folder(&self, path: &str, has_sub: bool) -> Node {
        Node::Folder {
            path: path.to_string(),
            seg: seg_of(path).to_string(),
            has_sub,
        }
    }

    fn sessions_of(&self, path: &str, depth: usize, out: &mut Vec<(usize, Node)>) {
        for &i in self.at.get(path).into_iter().flatten() {
            out.push((depth, Node::Session(i)));
        }
    }

    fn walk(&self, path: &str, depth: usize, out: &mut Vec<(usize, Node)>) {
        let subs = self.kids.get(path).map_or(0, BTreeSet::len);
        out.push((depth, self.folder(path, subs > 0)));
        self.sessions_of(path, depth + 1, out);

        if !self.collapsed.contains(path) {
            for c in self.kids.get(path).into_iter().flatten() {
                self.walk(c, depth + 1, out);
            }
            return;
        }
        // Collapsed: keep only the folders that actually hold sessions and stand the
        // pass-through ones we dropped up as a single "…".
        let (mut keep, mut elided) = (Vec::new(), false);
        self.descend(path, &mut keep, &mut elided);
        if keep.is_empty() {
            return;
        }
        keep.sort();
        // ponytail: one "…" for the whole subtree, and the survivors show only their last
        // segment — right for the chain this is meant for, lossy on a wide tree. Give each
        // survivor its path relative to the collapsed folder if that ever reads wrong.
        let base = if elided {
            out.push((depth + 1, Node::Elide));
            depth + 2
        } else {
            depth + 1
        };
        for p in keep {
            out.push((base, self.folder(&p, false)));
            self.sessions_of(&p, base + 1, out);
        }
    }

    /// Every session-bearing descendant of `path`; `elided` records whether anything
    /// else was passed over on the way.
    fn descend(&self, path: &str, keep: &mut Vec<String>, elided: &mut bool) {
        for c in self.kids.get(path).into_iter().flatten() {
            if self.at.contains_key(c) {
                keep.push(c.clone());
            } else {
                *elided = true;
            }
            self.descend(c, keep, elided);
        }
    }
}

/// One VM's sidebar rows, as (depth, node). `idx` is the filter-surviving session
/// indices, so a folder with nothing left in it simply never gets built.
pub fn tree(
    sessions: &[SessionInfo],
    idx: &[usize],
    collapsed: &HashSet<String>,
) -> Vec<(usize, Node)> {
    let mut at: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut kids: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    for &i in idx {
        let mut path = String::new();
        for (d, seg) in segments(&sessions[i].cwd).iter().enumerate() {
            let parent = path.clone();
            path = join(&path, seg);
            if d == 0 {
                roots.insert(path.clone());
            } else {
                kids.entry(parent).or_default().insert(path.clone());
            }
            kids.entry(path.clone()).or_default();
        }
        at.entry(path).or_default().push(i);
    }

    let t = Tree {
        at: &at,
        kids: &kids,
        collapsed,
    };
    let mut out = Vec::new();
    for r in &roots {
        t.walk(r, 0, &mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::Status;

    fn sess(cwd: &str) -> SessionInfo {
        SessionInfo {
            id: cwd.into(),
            agent: "claude".into(),
            name: "x".into(),
            cwd: cwd.into(),
            status: Status::Stopped,
            created_at: 0,
        }
    }

    /// (depth, label) per row — folders by segment, sessions as `#index`.
    fn shape(rows: &[(usize, Node)]) -> Vec<(usize, String)> {
        rows.iter()
            .map(|(d, n)| {
                (
                    *d,
                    match n {
                        Node::Folder { seg, .. } => seg.clone(),
                        Node::Elide => "…".into(),
                        Node::Session(i) => format!("#{i}"),
                    },
                )
            })
            .collect()
    }

    fn want(rows: &[(usize, &str)]) -> Vec<(usize, String)> {
        rows.iter().map(|(d, s)| (*d, s.to_string())).collect()
    }

    #[test]
    fn segments_normalize_paths() {
        std::env::set_var("HOME", "/Users/d");
        assert_eq!(segments("/Users/d/Documents/x"), ["~", "Documents", "x"]);
        assert_eq!(segments("/Users/d"), ["~"]);
        assert_eq!(segments("/Users/d/"), ["~"]);
        assert_eq!(segments("~"), ["~"]);
        assert_eq!(segments("~/a/b"), ["~", "a", "b"]);
        assert_eq!(segments(""), ["~"]);
        assert_eq!(segments("/etc/nginx"), ["/", "etc", "nginx"]);
        // A sibling of $HOME is not $HOME — prefix matching alone would get this wrong.
        assert_eq!(segments("/Users/dx/a"), ["/", "Users", "dx", "a"]);
    }

    #[test]
    fn tree_groups_sessions_by_cwd() {
        let s = [sess("~/Documents/agents-hub"), sess("~/Documents/notes")];
        assert_eq!(
            shape(&tree(&s, &[0, 1], &HashSet::new())),
            want(&[
                (0, "~"),
                (1, "Documents"),
                (2, "agents-hub"),
                (3, "#0"),
                (2, "notes"),
                (3, "#1"),
            ])
        );
    }

    #[test]
    fn collapse_elides_the_middle() {
        let s = [sess("~/Documents/f1/f2/f3")];
        let hidden = HashSet::from(["~/Documents".to_string()]);
        assert_eq!(
            shape(&tree(&s, &[0], &hidden)),
            want(&[(0, "~"), (1, "Documents"), (2, "…"), (3, "f3"), (4, "#0")])
        );
    }

    #[test]
    fn collapse_without_a_middle_emits_no_elide() {
        let s = [sess("~/Documents/a"), sess("~/Documents/b")];
        let hidden = HashSet::from(["~/Documents".to_string()]);
        assert_eq!(
            shape(&tree(&s, &[0, 1], &hidden)),
            want(&[
                (0, "~"),
                (1, "Documents"),
                (2, "a"),
                (3, "#0"),
                (2, "b"),
                (3, "#1"),
            ])
        );
    }

    #[test]
    fn filtered_out_sessions_take_their_folders_with_them() {
        let s = [sess("~/Documents/a"), sess("/etc/nginx")];
        assert_eq!(
            shape(&tree(&s, &[1], &HashSet::new())),
            want(&[(0, "/"), (1, "etc"), (2, "nginx"), (3, "#1")])
        );
    }
}
