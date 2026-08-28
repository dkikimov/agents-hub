//! Client state: the VMs, the sidebar rows derived from them, and one vt100 parser
//! per session. Queries and invariants live here; the event handlers that mutate it
//! live in `event`, and everything that paints it in `render`.

use super::clipboard::Clipboard;
use super::tree::{tree, Node};
use super::SIDE_W;
use crate::proto::{Req, SessionInfo};
use ratatui::crossterm::event::MouseEvent;
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::sync::mpsc::UnboundedSender;
use tui_term::vt100;

/// Key for everything held per session: a session id is only unique within its VM.
pub type Pane = (usize, String);

pub struct VmState {
    pub name: String,
    pub local: bool,
    pub online: bool,
    pub sessions: Vec<SessionInfo>,
    pub tx: UnboundedSender<Req>,
}

/// Sidebar rows. `Copy`, so anything with a payload lives in `App::folders` instead.
#[derive(Clone, Copy)]
pub enum Row {
    Vm(usize),
    /// Index into `App::folders`.
    Folder(usize),
    /// The inert "…" placeholder, at this depth.
    Elide(usize),
    Session(usize, usize, usize), // vm, session, depth
}

pub struct Folder {
    pub vm: usize,
    pub path: String,
    pub seg: String,
    pub depth: usize,
    pub collapsed: bool,
    pub has_sub: bool,
}

#[derive(PartialEq)]
pub enum Focus {
    Sidebar,
    Terminal,
}

pub enum Modal {
    New {
        vm: usize,
        agent: usize,
        name: String,
        cwd: String,
        field: u8,
    },
    Kill {
        vm: usize,
        id: String,
        label: String,
    },
    Help,
}

/// A drag over the pane. `down` is the press that started it, kept so a drag that
/// never moved can still be replayed to the guest as a click.
pub struct Selection {
    pub vm: usize,
    pub id: String,
    pub down: Option<MouseEvent>,
    pub start: (u16, u16),
    pub end: (u16, u16),
}

pub struct App {
    pub vms: Vec<VmState>,
    pub rows: Vec<Row>,
    pub folders: Vec<Folder>,
    /// (vm index, folder path) the user hid with space. Keyed by path, not row index,
    /// so it survives the rebuild that every session list update triggers.
    // ponytail: in-memory only, so folds reset on restart. Park it in state.json if that bites.
    pub collapsed: HashSet<(usize, String)>,
    pub sel: usize,
    pub focus: Focus,
    pub panes: HashMap<Pane, vt100::Parser<Clipboard>>,
    pub attached: HashSet<Pane>,
    pub activity: HashMap<Pane, Instant>,
    pub modal: Option<Modal>,
    pub filter: String,
    pub editing_filter: bool,
    pub agents: Vec<String>,
    pub pane: (u16, u16),     // cols, rows
    pub pane_org: (u16, u16), // top-left of the pane on screen, for mouse coords
    pub side_w: u16,
    /// Top-left of the sidebar list, and the row it's scrolled to — together they turn a
    /// click's y into a `rows` index. The offset persists so the list doesn't jump.
    pub side_org: (u16, u16),
    pub side_off: usize,
    /// Dragging the border between sidebar and pane.
    pub drag_split: bool,
    pub selection: Option<Selection>,
    pub status: String,
    pub dirty: bool,
}

impl App {
    pub fn new(vms: Vec<VmState>, agents: Vec<String>) -> App {
        let mut app = App {
            vms,
            rows: Vec::new(),
            folders: Vec::new(),
            collapsed: HashSet::new(),
            sel: 0,
            focus: Focus::Sidebar,
            panes: HashMap::new(),
            attached: HashSet::new(),
            activity: HashMap::new(),
            modal: None,
            filter: String::new(),
            editing_filter: false,
            agents,
            pane: (80, 24),
            pane_org: (0, 0),
            side_w: SIDE_W,
            side_org: (0, 0),
            side_off: 0,
            drag_split: false,
            selection: None,
            status: String::new(),
            dirty: true,
        };
        app.rebuild();
        app
    }

    /// Recomputes the sidebar from the VM lists, the filter, and the folds. Cheap
    /// enough to run on every session-list update, which is what keeps it correct.
    pub fn rebuild(&mut self) {
        let mut rows = std::mem::take(&mut self.rows);
        let mut folders = std::mem::take(&mut self.folders);
        rows.clear();
        folders.clear();
        let f = self.filter.to_lowercase();
        for (vi, vm) in self.vms.iter().enumerate() {
            rows.push(Row::Vm(vi));
            let idx: Vec<usize> = vm
                .sessions
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    f.is_empty()
                        || s.name.to_lowercase().contains(&f)
                        || s.agent.to_lowercase().contains(&f)
                        || s.cwd.to_lowercase().contains(&f)
                })
                .map(|(si, _)| si)
                .collect();
            let hidden: HashSet<String> = self
                .collapsed
                .iter()
                .filter(|(v, _)| *v == vi)
                .map(|(_, p)| p.clone())
                .collect();
            for (depth, node) in tree(&vm.sessions, &idx, &hidden) {
                match node {
                    Node::Folder { path, seg, has_sub } => {
                        rows.push(Row::Folder(folders.len()));
                        folders.push(Folder {
                            vm: vi,
                            collapsed: hidden.contains(&path),
                            path,
                            seg,
                            depth,
                            has_sub,
                        });
                    }
                    Node::Elide => rows.push(Row::Elide(depth)),
                    Node::Session(si) => rows.push(Row::Session(vi, si, depth)),
                }
            }
        }
        self.rows = rows;
        self.folders = folders;
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    pub fn cur(&self) -> Option<(usize, &SessionInfo)> {
        match self.rows.get(self.sel)? {
            Row::Session(vi, si, _) => Some((*vi, self.vms[*vi].sessions.get(*si)?)),
            _ => None,
        }
    }

    /// The selected session as (vm, id, is-running) — what nearly every handler wants.
    pub fn cur_live(&self) -> Option<(usize, String, bool)> {
        self.cur()
            .map(|(vi, s)| (vi, s.id.clone(), s.status == crate::proto::Status::Running))
    }

    pub fn cur_vm(&self) -> usize {
        match self.rows.get(self.sel) {
            Some(Row::Vm(vi)) | Some(Row::Session(vi, _, _)) => *vi,
            Some(Row::Folder(fi)) => self.folders[*fi].vm,
            _ => 0,
        }
    }

    /// Moves the selection one row, stepping over the inert "…" placeholders.
    pub fn step(&mut self, down: bool) {
        let last = self.rows.len().saturating_sub(1);
        loop {
            match self.sel {
                s if down && s < last => self.sel = s + 1,
                s if !down && s > 0 => self.sel = s - 1,
                _ => return,
            }
            if !matches!(self.rows[self.sel], Row::Elide(_)) {
                return;
            }
        }
    }

    /// cwd to seed a new session with: the folder you're standing in, or the one the
    /// selected session runs in, else this VM's default.
    pub fn new_cwd(&self) -> String {
        match self.rows.get(self.sel) {
            Some(Row::Folder(fi)) => self.folders[*fi].path.clone(),
            Some(Row::Session(vi, si, _)) => self.vms[*vi].sessions[*si].cwd.clone(),
            _ if self.vms[self.cur_vm()].local => std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~".into()),
            _ => "~".into(),
        }
    }

    pub fn send(&mut self, vm: usize, req: Req) {
        if let Some(v) = self.vms.get(vm) {
            if v.tx.send(req).is_err() {
                self.status = format!("{}: link closed", v.name);
            }
        }
    }

    /// Attach to every session we haven't seen yet and drop panes for ones that
    /// vanished. Staying attached to all of them means switching selection is
    /// instant instead of triggering a fresh 128 KB replay.
    pub fn reconcile(&mut self, vi: usize) {
        let (cols, rows) = self.pane;
        let live: HashSet<String> = self.vms[vi].sessions.iter().map(|s| s.id.clone()).collect();
        self.panes.retain(|(v, id), _| *v != vi || live.contains(id));
        self.attached.retain(|(v, id)| *v != vi || live.contains(id));
        self.activity.retain(|(v, id), _| *v != vi || live.contains(id));
        if self
            .selection
            .as_ref()
            .is_some_and(|s| !self.panes.contains_key(&(s.vm, s.id.clone())))
        {
            self.selection = None;
        }

        let fresh: Vec<String> = live
            .iter()
            .filter(|id| !self.attached.contains(&(vi, (*id).clone())))
            .cloned()
            .collect();
        for id in fresh {
            self.panes.insert(
                (vi, id.clone()),
                vt100::Parser::new_with_callbacks(rows, cols, 2000, Clipboard::default()),
            );
            self.attached.insert((vi, id.clone()));
            self.send(vi, Req::Attach { id, cols, rows });
        }
    }

    pub fn resize_panes(&mut self, cols: u16, rows: u16) {
        if self.pane == (cols, rows) {
            return;
        }
        self.pane = (cols, rows);
        self.selection = None;
        for p in self.panes.values_mut() {
            p.screen_mut().set_size(rows, cols);
        }
        let targets: Vec<Pane> = self.attached.iter().cloned().collect();
        for (vi, id) in targets {
            self.send(vi, Req::Resize { id, cols, rows });
        }
    }
}

/// "~/Documents/agents-hub" → "agents-hub", so a session gets a useful name for free.
pub fn default_name(cwd: &str, agent: &str) -> String {
    match cwd.trim_end_matches('/').rsplit('/').next() {
        Some(dir) if !dir.is_empty() && dir != "~" => dir.to_string(),
        _ => agent.to_string(),
    }
}

#[cfg(test)]
pub mod fixture {
    //! One local VM with a running session per cwd, and the channel the client would
    //! be writing requests into so a test can read back what it sent.

    use super::*;
    use crate::proto::Status;
    use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};

    pub fn app(cwds: &[&str]) -> (App, UnboundedReceiver<Req>) {
        let (tx, rx) = unbounded_channel();
        let sessions = cwds
            .iter()
            .enumerate()
            .map(|(i, cwd)| SessionInfo {
                id: format!("s{i}"),
                agent: "claude".into(),
                name: default_name(cwd, "claude"),
                cwd: (*cwd).into(),
                status: Status::Running,
                created_at: i as u64,
            })
            .collect();
        let vm = VmState {
            name: "local".into(),
            local: true,
            online: true,
            sessions,
            tx,
        };
        (App::new(vec![vm], vec!["claude".into(), "codex".into()]), rx)
    }

    pub fn sent(rx: &mut UnboundedReceiver<Req>) -> Vec<Req> {
        let mut out = Vec::new();
        while let Ok(req) = rx.try_recv() {
            out.push(req);
        }
        out
    }

    /// (depth, label) per sidebar row, for asserting on shape rather than pixels.
    pub fn shape(app: &App) -> Vec<String> {
        app.rows
            .iter()
            .map(|r| match *r {
                Row::Vm(vi) => app.vms[vi].name.clone(),
                Row::Folder(fi) => {
                    let f = &app.folders[fi];
                    format!("{}{}/", "  ".repeat(f.depth), f.seg)
                }
                Row::Elide(d) => format!("{}…", "  ".repeat(d)),
                Row::Session(vi, si, d) => {
                    format!("{}{}", "  ".repeat(d), app.vms[vi].sessions[si].name)
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;

    #[test]
    fn default_name_is_the_folder() {
        assert_eq!(
            default_name("/Users/d/Documents/agents-hub", "claude"),
            "agents-hub"
        );
        assert_eq!(
            default_name("/Users/d/Documents/agents-hub/", "claude"),
            "agents-hub"
        );
        assert_eq!(default_name("~/src/foo", "claude"), "foo");
        // Nothing folder-shaped to use — fall back to the agent.
        assert_eq!(default_name("~", "claude"), "claude");
        assert_eq!(default_name("/", "claude"), "claude");
        assert_eq!(default_name("", "claude"), "claude");
    }

    #[test]
    fn step_walks_past_the_inert_elide_rows() {
        let (mut app, _rx) = app(&["~/a/b/c/d"]);
        app.collapsed.insert((0, "~/a".into()));
        app.rebuild();
        assert_eq!(shape(&app), ["local", "~/", "  a/", "    …", "      d/", "        d"]);

        // Down from "a/" must skip "…" and land on "d/".
        app.sel = 2;
        app.step(true);
        assert!(matches!(app.rows[app.sel], Row::Folder(_)));
        assert_eq!(app.sel, 4);
        app.step(false);
        assert_eq!(app.sel, 2);
        // And it stops at the ends rather than wrapping.
        app.sel = 0;
        app.step(false);
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn filter_drops_folders_that_lose_all_their_sessions() {
        let (mut app, _rx) = app(&["~/work/api", "~/play/game"]);
        app.filter = "game".into();
        app.rebuild();
        assert_eq!(shape(&app), ["local", "~/", "  play/", "    game/", "      game"]);
    }

    #[test]
    fn a_shrinking_list_never_leaves_the_selection_past_the_end() {
        let (mut app, _rx) = app(&["~/work/api", "~/play/game"]);
        app.sel = app.rows.len() - 1;
        app.vms[0].sessions.clear();
        app.rebuild();
        assert_eq!(app.sel, app.rows.len() - 1);
        assert!(app.cur().is_none());
    }

    #[test]
    fn new_cwd_follows_whatever_is_selected() {
        let (mut app, _rx) = app(&["~/work/api"]);
        // rows: local, ~/, work/, api/, <session>
        app.sel = 2;
        assert_eq!(app.new_cwd(), "~/work");
        app.sel = 4;
        assert_eq!(app.new_cwd(), "~/work/api");
        // On the VM row a local VM seeds from the client's own cwd.
        app.sel = 0;
        assert_eq!(
            app.new_cwd(),
            std::env::current_dir().unwrap().display().to_string()
        );
    }

    #[test]
    fn reconcile_attaches_new_sessions_and_forgets_dead_ones() {
        let (mut app, mut rx) = app(&["~/a"]);
        app.reconcile(0);
        assert_eq!(
            sent(&mut rx),
            [Req::Attach {
                id: "s0".into(),
                cols: 80,
                rows: 24
            }]
        );
        assert_eq!(app.panes.len(), 1);

        // Already attached: no second replay.
        app.reconcile(0);
        assert!(sent(&mut rx).is_empty());

        app.vms[0].sessions.clear();
        app.reconcile(0);
        assert!(app.panes.is_empty() && app.attached.is_empty());
    }

    #[test]
    fn resizing_tells_every_attached_session_once() {
        let (mut app, mut rx) = app(&["~/a"]);
        app.reconcile(0);
        sent(&mut rx);

        app.resize_panes(100, 40);
        assert_eq!(
            sent(&mut rx),
            [Req::Resize {
                id: "s0".into(),
                cols: 100,
                rows: 40
            }]
        );
        app.resize_panes(100, 40); // unchanged — nothing to say
        assert!(sent(&mut rx).is_empty());
        assert_eq!(app.panes[&(0, "s0".to_string())].screen().size(), (40, 100));
    }
}
