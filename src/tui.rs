//! The client. One task per configured VM (local socket or `ssh host agents-hub stdio`),
//! a vt100 parser per session, and a sidebar tree grouped by VM name.

use crate::config::{Config, Vm};
use crate::proto::*;
use anyhow::{bail, Result};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tui_term::widget::PseudoTerminal;
use tui_term::vt100;
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// One redraw per frame at most, however many bytes arrive in between. This is what
/// keeps the UI responsive when three agents stream at once.
const FRAME: Duration = Duration::from_millis(16);

/// Sidebar width: start, and the bounds the drag handle clamps to.
const SIDE_W: u16 = 30;
const SIDE_MIN: u16 = 14;
const PANE_MIN: u16 = 20;

/// Lines per wheel tick.
const WHEEL: usize = 3;

type Rd = Box<dyn AsyncRead + Unpin + Send>;
type Wr = Box<dyn AsyncWrite + Unpin + Send>;

#[derive(Default)]
struct Clipboard(Vec<Vec<u8>>);

impl vt100::Callbacks for Clipboard {
    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _: &[u8], data: &[u8]) {
        if let Ok(data) = std::str::from_utf8(data) {
            if let Ok(data) = unb64(data) {
                self.0.push(data);
            }
        }
    }
}

impl Clipboard {
    fn take(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.0)
    }

    fn take_if(&mut self, allowed: bool) -> Vec<Vec<u8>> {
        let copies = self.take();
        if allowed { copies } else { Vec::new() }
    }
}

fn copy_local(data: &[u8]) -> Result<()> {
    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()?;
    let Some(mut stdin) = child.stdin.take() else {
        bail!("pbcopy stdin unavailable")
    };
    stdin.write_all(data)?;
    drop(stdin);
    if !child.wait()?.success() {
        bail!("pbcopy failed")
    }
    Ok(())
}

enum Ui {
    Input(Event),
    Up(usize),
    Down(usize),
    Msg(usize, Resp),
}

// ── connection ────────────────────────────────────────────────────────────────

async fn link(vm: &Vm) -> Result<(Rd, Wr, Option<tokio::process::Child>)> {
    match &vm.ssh {
        None => {
            let (r, w) = crate::connect_local().await?.into_split();
            Ok((Box::new(r), Box::new(w), None))
        }
        Some(host) => {
            let mut child = tokio::process::Command::new("ssh")
                // Never let a password prompt hang the TUI: keys/agent only.
                .args(["-o", "BatchMode=yes", "-o", "ServerAliveInterval=20"])
                .arg(host)
                .arg(&vm.remote_bin)
                .arg("stdio")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let (Some(r), Some(w)) = (child.stdout.take(), child.stdin.take()) else {
                bail!("ssh pipes unavailable")
            };
            Ok((Box::new(r), Box::new(w), Some(child)))
        }
    }
}

async fn pump(idx: usize, r: Rd, mut w: Wr, tx: &UnboundedSender<Ui>, rx: &mut UnboundedReceiver<Req>) {
    let mut lines = BufReader::new(r).lines();
    loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) if !l.trim().is_empty() => {
                    if let Ok(resp) = serde_json::from_str::<Resp>(&l) {
                        if tx.send(Ui::Msg(idx, resp)).is_err() { return }
                    }
                }
                Ok(Some(_)) => {}
                _ => return,
            },
            req = rx.recv() => match req {
                Some(req) => {
                    let Ok(mut s) = serde_json::to_string(&req) else { continue };
                    s.push('\n');
                    if w.write_all(s.as_bytes()).await.is_err() { return }
                }
                None => return,
            },
        }
    }
}

/// Reconnects forever with capped backoff; a VM being down is a display state,
/// never a reason to exit.
async fn vm_task(idx: usize, vm: Vm, tx: UnboundedSender<Ui>, mut rx: UnboundedReceiver<Req>) {
    let mut backoff = 1u64;
    loop {
        if let Ok((r, w, child)) = link(&vm).await {
            backoff = 1;
            if tx.send(Ui::Up(idx)).is_err() {
                return;
            }
            pump(idx, r, w, &tx, &mut rx).await;
            drop(child); // kill_on_drop reaps ssh
        }
        if tx.send(Ui::Down(idx)).is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

// ── folder tree ───────────────────────────────────────────────────────────────

/// A cwd's tree segments, rooted at `~` when it sits under `$HOME`, else at `/`.
/// Applied to remote cwds too: the client can't know a remote `$HOME`, so a remote
/// `/home/u/x` roots at `/`, which is at least honest. Paths typed as `~/x` in the
/// new-session modal arrive as `~/x` and root correctly on either kind of VM.
fn segments(cwd: &str) -> Vec<String> {
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

enum Node {
    Folder { path: String, seg: String, has_sub: bool },
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
fn tree(sessions: &[SessionInfo], idx: &[usize], collapsed: &HashSet<String>) -> Vec<(usize, Node)> {
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

    let t = Tree { at: &at, kids: &kids, collapsed };
    let mut out = Vec::new();
    for r in &roots {
        t.walk(r, 0, &mut out);
    }
    out
}

// ── state ─────────────────────────────────────────────────────────────────────

struct VmState {
    name: String,
    local: bool,
    online: bool,
    sessions: Vec<SessionInfo>,
    tx: UnboundedSender<Req>,
}

/// Sidebar rows. `Copy`, so anything with a payload lives in `App::folders` instead.
#[derive(Clone, Copy)]
enum Row {
    Vm(usize),
    /// Index into `App::folders`.
    Folder(usize),
    /// The inert "…" placeholder, at this depth.
    Elide(usize),
    Session(usize, usize, usize), // vm, session, depth
}

struct Folder {
    vm: usize,
    path: String,
    seg: String,
    depth: usize,
    collapsed: bool,
    has_sub: bool,
}

#[derive(PartialEq)]
enum Focus {
    Sidebar,
    Terminal,
}

enum Modal {
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

struct Selection {
    vm: usize,
    id: String,
    down: Option<MouseEvent>,
    start: (u16, u16),
    end: (u16, u16),
}

struct App {
    vms: Vec<VmState>,
    rows: Vec<Row>,
    folders: Vec<Folder>,
    /// (vm index, folder path) the user hid with space. Keyed by path, not row index,
    /// so it survives the rebuild that every session list update triggers.
    // ponytail: in-memory only, so folds reset on restart. Park it in state.json if that bites.
    collapsed: HashSet<(usize, String)>,
    sel: usize,
    focus: Focus,
    panes: HashMap<(usize, String), vt100::Parser<Clipboard>>,
    attached: HashSet<(usize, String)>,
    modal: Option<Modal>,
    filter: String,
    editing_filter: bool,
    agents: Vec<String>,
    pane: (u16, u16), // cols, rows
    pane_org: (u16, u16), // top-left of the pane on screen, for mouse coords
    side_w: u16,
    /// Top-left of the sidebar list, and the row it's scrolled to — together they turn a
    /// click's y into a `rows` index. The offset persists so the list doesn't jump.
    side_org: (u16, u16),
    side_off: usize,
    /// Dragging the border between sidebar and pane.
    drag_split: bool,
    selection: Option<Selection>,
    status: String,
    dirty: bool,
}

impl App {
    fn rebuild(&mut self) {
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

    fn cur(&self) -> Option<(usize, &SessionInfo)> {
        match self.rows.get(self.sel)? {
            Row::Session(vi, si, _) => Some((*vi, self.vms[*vi].sessions.get(*si)?)),
            _ => None,
        }
    }

    fn cur_vm(&self) -> usize {
        match self.rows.get(self.sel) {
            Some(Row::Vm(vi)) | Some(Row::Session(vi, _, _)) => *vi,
            Some(Row::Folder(fi)) => self.folders[*fi].vm,
            _ => 0,
        }
    }

    /// Moves the selection one row, stepping over the inert "…" placeholders.
    fn step(&mut self, down: bool) {
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
    fn new_cwd(&self) -> String {
        match self.rows.get(self.sel) {
            Some(Row::Folder(fi)) => self.folders[*fi].path.clone(),
            Some(Row::Session(vi, si, _)) => self.vms[*vi].sessions[*si].cwd.clone(),
            _ if self.vms[self.cur_vm()].local => std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "~".into()),
            _ => "~".into(),
        }
    }

    fn send(&mut self, vm: usize, req: Req) {
        if let Some(v) = self.vms.get(vm) {
            if v.tx.send(req).is_err() {
                self.status = format!("{}: link closed", v.name);
            }
        }
    }

    /// Attach to every session we haven't seen yet and drop panes for ones that
    /// vanished. Staying attached to all of them means switching selection is
    /// instant instead of triggering a fresh 128 KB replay.
    fn reconcile(&mut self, vi: usize) {
        let (cols, rows) = self.pane;
        let live: HashSet<String> = self.vms[vi].sessions.iter().map(|s| s.id.clone()).collect();
        self.panes.retain(|(v, id), _| *v != vi || live.contains(id));
        self.attached.retain(|(v, id)| *v != vi || live.contains(id));
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
            self.panes
                .insert(
                    (vi, id.clone()),
                    vt100::Parser::new_with_callbacks(
                        rows,
                        cols,
                        2000,
                        Clipboard::default(),
                    ),
                );
            self.attached.insert((vi, id.clone()));
            self.send(vi, Req::Attach { id, cols, rows });
        }
    }

    fn resize_panes(&mut self, cols: u16, rows: u16) {
        if self.pane == (cols, rows) {
            return;
        }
        self.pane = (cols, rows);
        self.selection = None;
        for p in self.panes.values_mut() {
            p.screen_mut().set_size(rows, cols);
        }
        let targets: Vec<(usize, String)> = self.attached.iter().cloned().collect();
        for (vi, id) in targets {
            self.send(vi, Req::Resize { id, cols, rows });
        }
    }
}

// ── input ─────────────────────────────────────────────────────────────────────

/// crossterm key → the bytes a real terminal would have sent.
fn key_bytes(k: KeyEvent) -> Option<Vec<u8>> {
    use KeyCode::*;
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    let mut out: Vec<u8> = Vec::new();
    match k.code {
        Char(c) => {
            if ctrl {
                out.push(match c {
                    ' ' | '@' => 0,
                    'a'..='z' => c as u8 - b'a' + 1,
                    'A'..='Z' => c as u8 - b'A' + 1,
                    // Without the kitty keyboard protocol, crossterm reports bytes
                    // 0x1c..=0x1f as Ctrl+'4'..='7' — forward them as the real bytes.
                    '4'..='7' => c as u8 - b'4' + 0x1c,
                    '[' => 27,
                    '\\' => 28,
                    ']' => 29,
                    '^' => 30,
                    '_' | '?' => 31,
                    _ => return None,
                });
            } else {
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
        Enter => out.push(b'\r'),
        Tab => out.push(b'\t'),
        BackTab => out.extend_from_slice(b"\x1b[Z"),
        Backspace => out.push(0x7f),
        Esc => out.push(0x1b),
        Up => out.extend_from_slice(b"\x1b[A"),
        Down => out.extend_from_slice(b"\x1b[B"),
        Right => out.extend_from_slice(b"\x1b[C"),
        Left => out.extend_from_slice(b"\x1b[D"),
        Home => out.extend_from_slice(b"\x1b[H"),
        End => out.extend_from_slice(b"\x1b[F"),
        PageUp => out.extend_from_slice(b"\x1b[5~"),
        PageDown => out.extend_from_slice(b"\x1b[6~"),
        Delete => out.extend_from_slice(b"\x1b[3~"),
        Insert => out.extend_from_slice(b"\x1b[2~"),
        F(n) => match n {
            1..=4 => out.extend_from_slice(&[0x1b, b'O', b'P' + (n - 1)]),
            5 => out.extend_from_slice(b"\x1b[15~"),
            6..=10 => out.extend_from_slice(format!("\x1b[{}~", n + 11).as_bytes()),
            11..=12 => out.extend_from_slice(format!("\x1b[{}~", n + 12).as_bytes()),
            _ => return None,
        },
        _ => return None,
    }
    if alt {
        out.insert(0, 0x1b);
    }
    Some(out)
}

/// A paste → the bytes a real terminal would have sent. `bracketed` mirrors the *guest's*
/// mode: an app that asked for `?2004h` gets one paste it can hold as a block, anything else
/// gets the lines raw, exactly as before. The `\x1b[201~` strip matters — pasted content
/// containing the end marker would otherwise close the bracket early and have its tail read
/// as typed input, which in an agent prompt means submitting.
fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text.replace("\r\n", "\r").replace('\n', "\r").replace("\x1b[201~", "");
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

fn on_paste(app: &mut App, text: &str) {
    app.dirty = true;
    app.selection = None;

    // Single-line text fields: replay as keys so the modal/filter accumulators stay the
    // only place that knows how they're edited.
    if app.modal.is_some() || app.editing_filter {
        for c in text.chars().filter(|c| !c.is_control()) {
            on_key(app, KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        return;
    }
    // In the sidebar a paste would run single-letter commands ('q' quits). Drop it.
    if app.focus != Focus::Terminal {
        return;
    }
    let Some((vi, s)) = app.cur() else { return };
    let (id, running) = (s.id.clone(), s.status == Status::Running);
    if !running {
        return;
    }
    let Some(p) = app.panes.get_mut(&(vi, id.clone())) else { return };
    let bracketed = p.screen().bracketed_paste();
    p.screen_mut().set_scrollback(0);
    // ponytail: sent as one frame, and the daemon's write_all to the pty is blocking — a
    // multi-MB paste stalls that client's connection task. Chunk it if that ever bites.
    let data = b64(&paste_bytes(text, bracketed));
    app.send(vi, Req::Input { id, data });
}

/// Returns false when the app should quit.
fn on_key(app: &mut App, k: KeyEvent) -> bool {
    app.dirty = true;
    app.selection = None;

    if let Some(modal) = app.modal.take() {
        return modal_key(app, modal, k);
    }

    if app.editing_filter {
        match k.code {
            KeyCode::Esc => {
                app.filter.clear();
                app.editing_filter = false;
            }
            KeyCode::Enter => app.editing_filter = false,
            KeyCode::Backspace => {
                app.filter.pop();
            }
            KeyCode::Char(c) => app.filter.push(c),
            _ => {}
        }
        app.rebuild();
        return true;
    }

    if app.focus == Focus::Terminal {
        // Ctrl-] is the one key the pane doesn't get: it's the way back out.
        // Byte 0x1d reaches us as Ctrl+'5' on terminals without the kitty protocol.
        let escape = matches!(k.code, KeyCode::Char(']') | KeyCode::Char('5'))
            && k.modifiers.contains(KeyModifiers::CONTROL);
        if escape {
            app.focus = Focus::Sidebar;
            return true;
        }
        if let Some((vi, s)) = app.cur() {
            let (id, running) = (s.id.clone(), s.status == Status::Running);
            if running {
                if let Some(bytes) = key_bytes(k) {
                    // Typing jumps back to the live bottom, like every other terminal.
                    if let Some(p) = app.panes.get_mut(&(vi, id.clone())) {
                        p.screen_mut().set_scrollback(0);
                    }
                    app.send(vi, Req::Input { id, data: b64(&bytes) });
                }
            }
        }
        return true;
    }

    match k.code {
        KeyCode::Char('q') => return false,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Char('j') | KeyCode::Down => app.step(true),
        KeyCode::Char('k') | KeyCode::Up => app.step(false),
        KeyCode::Char('g') | KeyCode::Home => app.sel = 0,
        KeyCode::Char('G') | KeyCode::End => app.sel = app.rows.len().saturating_sub(1),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
            if app.cur().is_some() {
                app.focus = Focus::Terminal;
            }
        }
        KeyCode::Char('/') => {
            app.editing_filter = true;
            app.filter.clear();
        }
        KeyCode::Char('?') => app.modal = Some(Modal::Help),
        // Collapsing only drops rows *below* the folder, so the selection stays put.
        KeyCode::Char(' ') => {
            if let Some(Row::Folder(fi)) = app.rows.get(app.sel).copied() {
                let f = &app.folders[fi];
                if f.has_sub {
                    let key = (f.vm, f.path.clone());
                    if !app.collapsed.remove(&key) {
                        app.collapsed.insert(key);
                    }
                    app.rebuild();
                }
            }
        }
        KeyCode::Char('n') => {
            app.modal = Some(Modal::New {
                vm: app.cur_vm(),
                agent: 0,
                name: String::new(),
                cwd: app.new_cwd(),
                field: 0,
            });
        }
        KeyCode::Char('d') => {
            if let Some((vi, s)) = app.cur() {
                app.modal = Some(Modal::Kill {
                    vm: vi,
                    id: s.id.clone(),
                    label: format!("{} · {}", s.agent, s.name),
                });
            }
        }
        KeyCode::Char('r') => {
            if let Some((vi, s)) = app.cur() {
                if s.status == Status::Stopped {
                    let id = s.id.clone();
                    let (cols, rows) = app.pane;
                    app.send(vi, Req::Restart { id, cols, rows });
                }
            }
        }
        _ => {}
    }
    true
}

fn pane_cell(
    (ox, oy): (u16, u16),
    (cols, rows): (u16, u16),
    (x, y): (u16, u16),
    clamp: bool,
) -> Option<(u16, u16)> {
    if cols == 0 || rows == 0 {
        return None;
    }
    if clamp {
        return Some((
            x.saturating_sub(ox).min(cols - 1),
            y.saturating_sub(oy).min(rows - 1),
        ));
    }
    let (col, row) = (x.checked_sub(ox)?, y.checked_sub(oy)?);
    (col < cols && row < rows).then_some((col, row))
}

fn selected_text(
    screen: &vt100::Screen,
    start: (u16, u16),
    end: (u16, u16),
) -> Option<String> {
    if start == end {
        return None;
    }
    let (rows, cols) = screen.size();
    if [start, end]
        .into_iter()
        .any(|(col, row)| col >= cols || row >= rows)
    {
        return None;
    }
    let (start, end) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    let text = screen.contents_between(
        start.1,
        start.0,
        end.1,
        end.0.saturating_add(1).min(cols),
    );
    (!text.is_empty()).then_some(text)
}

/// crossterm mouse event → the bytes a real terminal would send, in whatever protocol
/// the app inside the pane turned on. `None` means that app didn't ask for this event
/// (or for any mouse at all), and we should handle it ourselves.
fn mouse_bytes(m: MouseEvent, screen: &vt100::Screen, col: u16, row: u16) -> Option<Vec<u8>> {
    use MouseEventKind::*;
    let mode = screen.mouse_protocol_mode();
    let motion = matches!(m.kind, Drag(_) | Moved);
    let wanted = match mode {
        MouseProtocolMode::None => false,
        // X10 reports presses only — no release, no motion.
        MouseProtocolMode::Press => !matches!(m.kind, Up(_)) && !motion,
        MouseProtocolMode::PressRelease => !motion,
        MouseProtocolMode::ButtonMotion => !matches!(m.kind, Moved),
        MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return None;
    }

    let button = |b| match b {
        ratatui::crossterm::event::MouseButton::Left => 0,
        ratatui::crossterm::event::MouseButton::Middle => 1,
        ratatui::crossterm::event::MouseButton::Right => 2,
    };
    let mut cb: u16 = match m.kind {
        Down(b) | Up(b) => button(b),
        Drag(b) => button(b) + 32,
        Moved => 3 + 32, // motion with no button held
        ScrollUp => 64,
        ScrollDown => 65,
        ScrollLeft => 66,
        ScrollRight => 67,
    };
    cb += u16::from(m.modifiers.contains(KeyModifiers::SHIFT)) * 4
        + u16::from(m.modifiers.contains(KeyModifiers::ALT)) * 8
        + u16::from(m.modifiers.contains(KeyModifiers::CONTROL)) * 16;

    let (x, y) = (col + 1, row + 1); // the wire protocol is 1-based
    Some(match screen.mouse_protocol_encoding() {
        MouseProtocolEncoding::Sgr => {
            let end = if matches!(m.kind, Up(_)) { 'm' } else { 'M' };
            format!("\x1b[<{cb};{x};{y}{end}").into_bytes()
        }
        // ponytail: the pre-SGR encodings cap coordinates at 223 and can't name the
        // button on release — fine, since nothing written this decade asks for them.
        _ => {
            let cb = if matches!(m.kind, Up(_)) { 3 } else { cb };
            let byte = |v: u16| (32 + v).min(255) as u8;
            vec![0x1b, b'[', b'M', byte(cb), byte(x), byte(y)]
        }
    })
}

fn click_bytes(
    down: MouseEvent,
    up: MouseEvent,
    screen: &vt100::Screen,
    (col, row): (u16, u16),
) -> Vec<u8> {
    [down, up]
        .into_iter()
        .filter_map(|m| mouse_bytes(m, screen, col, row))
        .flatten()
        .collect()
}

/// The two border columns between sidebar and pane, so the drag handle is a two-cell
/// target from either side.
fn on_split(col: u16, side_w: u16) -> bool {
    col + 1 >= side_w && col <= side_w
}

/// Screen row of a sidebar click → the row it landed on. `top` is the list's first line
/// (under the border) and `off` how far it's scrolled; the inert "…" rows aren't clickable.
fn row_at(rows: &[Row], off: usize, top: u16, y: u16) -> Option<usize> {
    let i = off + usize::from(y.checked_sub(top)?);
    match rows.get(i)? {
        Row::Elide(_) => None,
        _ => Some(i),
    }
}

/// Mouse over the sidebar selects and scrolls; the border between the two panes drags to
/// resize. Over the pane, the app inside gets the event if it turned mouse reporting on
/// (Claude Code does — that's how its own chat scrolls); otherwise the wheel walks our
/// vt100 scrollback instead.
fn on_mouse(app: &mut App, m: MouseEvent) {
    if app.modal.is_some() {
        return;
    }
    let left_down = matches!(
        m.kind,
        MouseEventKind::Down(ratatui::crossterm::event::MouseButton::Left)
    );
    let left_drag = matches!(
        m.kind,
        MouseEventKind::Drag(ratatui::crossterm::event::MouseButton::Left)
    );
    let left_up = matches!(
        m.kind,
        MouseEventKind::Up(ratatui::crossterm::event::MouseButton::Left)
    );
    let wheel = match m.kind {
        MouseEventKind::ScrollUp => Some(true),
        MouseEventKind::ScrollDown => Some(false),
        _ => None,
    };

    if app.selection.as_ref().is_some_and(|s| s.down.is_some()) && (left_drag || left_up) {
        let Some(cell) = pane_cell(
            app.pane_org,
            app.pane,
            (m.column, m.row),
            true,
        ) else {
            return;
        };
        if left_drag {
            if let Some(selection) = app.selection.as_mut() {
                selection.end = cell;
            }
            app.dirty = true;
            return;
        }

        let mut selection = app.selection.take().expect("active selection");
        selection.end = cell;
        let text = app
            .panes
            .get(&(selection.vm, selection.id.clone()))
            .and_then(|p| selected_text(p.screen(), selection.start, selection.end));
        if let Some(text) = text {
            app.status = match copy_local(text.as_bytes()) {
                Ok(()) => format!("copied {} chars", text.chars().count()),
                Err(e) => format!("copy failed: {e}"),
            };
            selection.down = None;
            app.selection = Some(selection);
        } else {
            let running = app.vms[selection.vm]
                .sessions
                .iter()
                .find(|s| s.id == selection.id)
                .is_some_and(|s| s.status == Status::Running);
            let bytes = app
                .panes
                .get(&(selection.vm, selection.id.clone()))
                .map(|p| {
                    click_bytes(
                        selection.down.expect("active selection"),
                        m,
                        p.screen(),
                        selection.start,
                    )
                })
                .unwrap_or_default();
            if running {
                app.focus = Focus::Terminal;
                if !bytes.is_empty() {
                    app.send(
                        selection.vm,
                        Req::Input {
                            id: selection.id,
                            data: b64(&bytes),
                        },
                    );
                }
            }
        }
        app.dirty = true;
        return;
    }

    if app.drag_split {
        match m.kind {
            MouseEventKind::Drag(_) => {
                app.side_w = (m.column + 1).max(SIDE_MIN); // draw clamps the far edge
                app.dirty = true;
            }
            _ => app.drag_split = false,
        }
        return;
    }
    if left_down && on_split(m.column, app.side_w) {
        app.selection = None;
        app.drag_split = true;
        return;
    }

    if m.column < app.side_w {
        if left_down || wheel.is_some() {
            app.selection = None;
        }
        if let Some(up) = wheel {
            app.dirty = true;
            app.step(!up);
        }
        if let Some(i) = left_down
            .then(|| row_at(&app.rows, app.side_off, app.side_org.1, m.row))
            .flatten()
        {
            // A click on a session keeps terminal focus, so switching chats mid-typing
            // doesn't cost a second keypress; anything else has nothing to type into.
            app.sel = i;
            if !matches!(app.rows[i], Row::Session(..)) {
                app.focus = Focus::Sidebar;
            }
            app.dirty = true;
        }
        return;
    }

    let Some((col, row)) = pane_cell(
        app.pane_org,
        app.pane,
        (m.column, m.row),
        false,
    ) else {
        return; // on the border or the status bar
    };
    let Some((vi, id, running)) = app
        .cur()
        .map(|(vi, s)| (vi, s.id.clone(), s.status == Status::Running))
    else {
        return;
    };

    if left_down
        && m.modifiers == KeyModifiers::NONE
        && app.panes.contains_key(&(vi, id.clone()))
    {
        app.selection = Some(Selection {
            vm: vi,
            id,
            down: Some(m),
            start: (col, row),
            end: (col, row),
        });
        app.dirty = true;
        return;
    }
    if wheel.is_some() {
        app.selection = None;
    }

    let bytes = app
        .panes
        .get(&(vi, id.clone()))
        .and_then(|p| mouse_bytes(m, p.screen(), col, row));
    if running {
        if matches!(m.kind, MouseEventKind::Down(_)) {
            app.focus = Focus::Terminal; // clicking in means you meant to type there
            app.dirty = true;
        }
        if let Some(bytes) = bytes {
            app.send(vi, Req::Input { id, data: b64(&bytes) });
            return;
        }
    }

    // The app doesn't want the mouse (plain shell, or the session is stopped), so the
    // wheel is ours: walk the scrollback. set_scrollback clamps to what's buffered.
    if let (Some(up), Some(p)) = (wheel, app.panes.get_mut(&(vi, id))) {
        app.dirty = true;
        let at = p.screen().scrollback();
        p.screen_mut()
            .set_scrollback(if up { at + WHEEL } else { at.saturating_sub(WHEEL) });
    }
}

/// "~/Documents/agents-hub" → "agents-hub", so a session gets a useful name for free.
fn default_name(cwd: &str, agent: &str) -> String {
    match cwd.trim_end_matches('/').rsplit('/').next() {
        Some(dir) if !dir.is_empty() && dir != "~" => dir.to_string(),
        _ => agent.to_string(),
    }
}

fn modal_key(app: &mut App, modal: Modal, k: KeyEvent) -> bool {
    match modal {
        Modal::Help => {
            if !matches!(k.code, KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q')) {
                app.modal = Some(Modal::Help);
            }
        }
        Modal::Kill { vm, id, label } => match k.code {
            KeyCode::Char('y') | KeyCode::Enter => {
                app.send(vm, Req::Kill { id });
                app.status = format!("killed {label}");
            }
            KeyCode::Char('n') | KeyCode::Esc => {}
            _ => app.modal = Some(Modal::Kill { vm, id, label }),
        },
        Modal::New {
            vm,
            mut agent,
            mut name,
            mut cwd,
            mut field,
        } => {
            let mut keep = true;
            match k.code {
                KeyCode::Esc => keep = false,
                KeyCode::Enter => {
                    let (cols, rows) = app.pane;
                    let agent_name = app.agents.get(agent).cloned().unwrap_or_default();
                    if agent_name.is_empty() {
                        app.status = "no [agents.*] in config.toml".into();
                    } else {
                        let display = if name.trim().is_empty() {
                            default_name(&cwd, &agent_name)
                        } else {
                            name.trim().to_string()
                        };
                        app.send(
                            vm,
                            Req::Create {
                                agent: agent_name,
                                name: display,
                                cwd: cwd.clone(),
                                cols,
                                rows,
                            },
                        );
                    }
                    keep = false;
                }
                KeyCode::Tab | KeyCode::Down => field = (field + 1) % 3,
                KeyCode::BackTab | KeyCode::Up => field = (field + 2) % 3,
                KeyCode::Left if field == 0 => {
                    agent = agent.checked_sub(1).unwrap_or(app.agents.len().saturating_sub(1))
                }
                KeyCode::Right if field == 0 => {
                    agent = if app.agents.is_empty() { 0 } else { (agent + 1) % app.agents.len() }
                }
                KeyCode::Backspace => {
                    if field == 1 {
                        name.pop();
                    } else if field == 2 {
                        cwd.pop();
                    }
                }
                KeyCode::Char(c) => {
                    if field == 1 {
                        name.push(c);
                    } else if field == 2 {
                        cwd.push(c);
                    }
                }
                _ => {}
            }
            if keep {
                app.modal = Some(Modal::New { vm, agent, name, cwd, field });
            }
        }
    }
    true
}

// ── rendering ─────────────────────────────────────────────────────────────────

fn sidebar_items(app: &App) -> Vec<ListItem<'static>> {
    app.rows
        .iter()
        .map(|row| match *row {
            Row::Vm(vi) => {
                let vm = &app.vms[vi];
                let mut spans = vec![
                    Span::styled("▾ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        vm.name.clone(),
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                    ),
                ];
                if !vm.online {
                    spans.push(Span::styled(
                        "  (offline)",
                        Style::default().fg(Color::Yellow),
                    ));
                }
                ListItem::new(Line::from(spans))
            }
            Row::Folder(fi) => {
                let f = &app.folders[fi];
                // A leaf gets a bullet rather than a caret: nothing to fold, but the
                // column still needs anchoring.
                let glyph = match (f.collapsed, f.has_sub) {
                    (true, _) => "▸ ",
                    (false, true) => "▾ ",
                    (false, false) => "· ",
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(f.depth + 1)),
                    Span::styled(glyph, Style::default().fg(Color::DarkGray)),
                    Span::styled(f.seg.clone(), Style::default().fg(Color::Blue)),
                ]))
            }
            Row::Elide(depth) => ListItem::new(Line::from(vec![
                Span::raw("  ".repeat(depth + 1)),
                Span::styled("…", Style::default().fg(Color::DarkGray)),
            ])),
            Row::Session(vi, si, depth) => {
                let vm = &app.vms[vi];
                let s = &vm.sessions[si];
                let (glyph, color) = match (vm.online, s.status) {
                    (false, _) => ("◌", Color::DarkGray),
                    (true, Status::Running) => ("●", Color::Green),
                    (true, Status::Stopped) => ("○", Color::DarkGray),
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(depth + 1)),
                    Span::styled(glyph, Style::default().fg(color)),
                    Span::raw(" "),
                    // Unpadded: the indent already groups these, and at depth the
                    // sidebar has no columns to spare.
                    Span::styled(s.agent.clone(), Style::default().fg(Color::Magenta)),
                    Span::raw(" "),
                    Span::raw(s.name.clone()),
                ]))
            }
        })
        .collect()
}

fn draw(f: &mut Frame, app: &mut App) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());
    // Clamped here rather than in the drag handler: this is where the terminal's width
    // is known, and Min() alone would let the two disagree about where the border is.
    let max = f.area().width.saturating_sub(PANE_MIN).max(SIDE_MIN);
    app.side_w = app.side_w.clamp(SIDE_MIN.min(max), max);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(app.side_w), Constraint::Min(PANE_MIN)])
        .split(outer[0]);

    // sidebar
    let side_focused = app.focus == Focus::Sidebar;
    let filter_active = app.editing_filter || !app.filter.is_empty();
    let title = if filter_active {
        format!(" filter: {}_ ", app.filter)
    } else {
        " VMs & Sessions ".into()
    };
    let list = List::new(sidebar_items(app))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(if filter_active {
                    Color::Yellow
                } else if side_focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(if filter_active { Color::Yellow } else { Color::Reset })
                        .add_modifier(if filter_active { Modifier::BOLD } else { Modifier::empty() }),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(if side_focused { Color::Blue } else { Color::DarkGray })
                .add_modifier(Modifier::BOLD),
        );
    let mut st = ListState::default().with_offset(app.side_off);
    st.select(Some(app.sel));
    f.render_stateful_widget(list, cols[0], &mut st);
    // Rendering clamps the offset and scrolls to the selection; keep what it settled on
    // so clicks map to the rows actually on screen.
    app.side_off = st.offset();
    app.side_org = (cols[0].x + 1, cols[0].y + 1);

    // terminal pane
    let sel = app.cur().map(|(vi, s)| (vi, s.clone()));
    let live = sel
        .as_ref()
        .is_some_and(|(vi, s)| s.status == Status::Running && app.vms[*vi].online);
    let pane_title = match &sel {
        Some((vi, s)) => format!(
            " {} {} · {} — {} ",
            if s.status == Status::Running { "●" } else { "○" },
            s.agent,
            s.name,
            app.vms[*vi].name
        ),
        None => " no session selected ".into(),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(if side_focused {
            Color::DarkGray
        } else {
            Color::Cyan
        }))
        .title(pane_title);
    let inner = block.inner(cols[1]);
    f.render_widget(block, cols[1]);

    app.pane_org = (inner.x, inner.y);
    app.resize_panes(inner.width.max(1), inner.height.max(1));

    match sel {
        Some((vi, s)) => match app.panes.get(&(vi, s.id.clone())) {
            Some(parser) => {
                let screen = parser.screen();
                f.render_widget(PseudoTerminal::new(screen), inner);
                if let Some(selection) = app.selection.as_ref().filter(|selection| {
                    selection.vm == vi && selection.id == s.id && selection.start != selection.end
                }) {
                    let (start, end) = if (selection.start.1, selection.start.0)
                        <= (selection.end.1, selection.end.0)
                    {
                        (selection.start, selection.end)
                    } else {
                        (selection.end, selection.start)
                    };
                    for row in start.1..=end.1 {
                        let first = if row == start.1 { start.0 } else { 0 };
                        let last = if row == end.1 {
                            end.0
                        } else {
                            inner.width.saturating_sub(1)
                        };
                        for col in first..=last {
                            f.buffer_mut()[(inner.x + col, inner.y + row)].set_style(
                                Style::default().add_modifier(Modifier::REVERSED),
                            );
                        }
                    }
                }
                if app.focus == Focus::Terminal && !screen.hide_cursor() {
                    let (r, c) = screen.cursor_position();
                    f.set_cursor_position((inner.x + c, inner.y + r));
                }
            }
            None => f.render_widget(Paragraph::new("connecting…"), inner),
        },
        None => f.render_widget(
            Paragraph::new("\n  Select a session, or press n to start one.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        ),
    }

    // Hints always visible; status rides the right edge so it never hides them.
    let hint = match (app.focus == Focus::Terminal, live) {
        // Never let keystrokes vanish into a dead pane without saying so.
        (true, false) => "  ^] back to list · session is not running — press r to restart",
        (true, true) => "  ^] back to list · keys go to the session",
        (false, _) => "  n new  d kill  r restart  ⏎ attach  / filter  ? help  q quit",
    };
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(34)])
        .split(outer[1]);
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        bar[0],
    );
    f.render_widget(
        Paragraph::new(app.status.clone())
            .right_aligned()
            .style(Style::default().fg(Color::DarkGray)),
        bar[1],
    );

    if let Some(m) = &app.modal {
        draw_modal(f, app, m);
    }
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn draw_modal(f: &mut Frame, app: &App, m: &Modal) {
    let (title, lines, w, h) = match m {
        Modal::Help => (
            " keys ",
            vec![
                Line::from("  j/k ↑↓   move            n   new session"),
                Line::from("  ⏎ / l    focus terminal  d   kill session"),
                Line::from("  ^]       back to list    r   restart stopped"),
                Line::from("  /        filter          q   quit"),
                Line::from("  space    fold a folder's middle away into …"),
                Line::from("  mouse    click a row to switch · drag the split to resize"),
                Line::from("           click pane controls · drag pane text to copy"),
                Line::from(""),
                Line::from(Span::styled(
                    "  folders come from each session's cwd; n starts one there",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            62,
            11,
        ),
        Modal::Kill { label, .. } => (
            " kill session ",
            vec![
                Line::from(format!("  {label}")),
                Line::from(""),
                Line::from(Span::styled(
                    "  y kill · n cancel",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            50,
            6,
        ),
        Modal::New {
            vm,
            agent,
            name,
            cwd,
            field,
        } => {
            let mark = |i: u8| if *field == i { "▸" } else { " " };
            let agent_name = app.agents.get(*agent).map(String::as_str).unwrap_or("—");
            (
                " new session ",
                vec![
                    Line::from(format!("  on {}", app.vms[*vm].name)),
                    Line::from(""),
                    Line::from(format!("{} agent  ← {} →", mark(0), agent_name)),
                    Line::from(vec![
                        Span::raw(format!("{} name   ", mark(1))),
                        if name.is_empty() {
                            // Dim, so it reads as the fallback rather than typed text.
                            Span::styled(
                                default_name(cwd, agent_name),
                                Style::default().fg(Color::DarkGray),
                            )
                        } else {
                            Span::raw(name.clone())
                        },
                    ]),
                    Line::from(format!("{} cwd    {}", mark(2), cwd)),
                    Line::from(""),
                    Line::from(Span::styled(
                        "  tab next field · ⏎ create · esc cancel",
                        Style::default().fg(Color::DarkGray),
                    )),
                ],
                60,
                10,
            )
        }
    };
    let area = centered(f.area(), w, h);
    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title(title),
        ),
        area,
    );
}

// ── main loop ─────────────────────────────────────────────────────────────────

pub async fn run() -> Result<()> {
    let cfg = Config::load(&crate::config_path())?;
    if cfg.vm.is_empty() {
        bail!("no [[vm]] entries in {}", crate::config_path().display());
    }

    let (tx, mut rx) = unbounded_channel::<Ui>();
    let mut vms = Vec::new();
    for (i, vm) in cfg.vm.iter().enumerate() {
        let (req_tx, req_rx) = unbounded_channel::<Req>();
        vms.push(VmState {
            name: vm.name.clone(),
            local: vm.ssh.is_none(),
            online: false,
            sessions: Vec::new(),
            tx: req_tx,
        });
        tokio::spawn(vm_task(i, vm.clone(), tx.clone(), req_rx));
    }

    // crossterm's read() is blocking; a plain thread is the whole adapter.
    let ktx = tx.clone();
    std::thread::spawn(move || loop {
        match ratatui::crossterm::event::read() {
            Ok(ev) => {
                if ktx.send(Ui::Input(ev)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });

    let mut app = App {
        vms,
        rows: Vec::new(),
        folders: Vec::new(),
        collapsed: HashSet::new(),
        sel: 0,
        focus: Focus::Sidebar,
        panes: HashMap::new(),
        attached: HashSet::new(),
        modal: None,
        filter: String::new(),
        editing_filter: false,
        agents: cfg.agent_names(),
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

    let mut term = ratatui::init();
    // Capture keeps pane/sidebar mouse features available; pane drags are copied locally.
    // Bracketed paste is what keeps a multi-line paste one message: without it the host
    // sends each line as a separate Enter, and an agent prompt submits on every one.
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste
    );
    let hook = std::panic::take_hook(); // ratatui's, which restores the screen
    std::panic::set_hook(Box::new(move |info| {
        let _ = ratatui::crossterm::execute!(
            std::io::stdout(),
            DisableMouseCapture,
            DisableBracketedPaste
        );
        hook(info);
    }));
    let mut ticker = tokio::time::interval(FRAME);
    let result = loop {
        tokio::select! {
            _ = ticker.tick() => {
                if app.dirty {
                    app.dirty = false;
                    if let Err(e) = term.draw(|f| draw(f, &mut app)) { break Err(e.into()) }
                }
            }
            ev = rx.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                match ev {
                    Ui::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if !on_key(&mut app, k) { break Ok(()) }
                    }
                    Ui::Input(Event::Mouse(m)) => on_mouse(&mut app, m),
                    Ui::Input(Event::Paste(text)) => on_paste(&mut app, &text),
                    Ui::Input(Event::Resize(..)) => {
                        app.selection = None;
                        app.dirty = true;
                    }
                    Ui::Input(_) => {}
                    Ui::Up(i) => {
                        app.vms[i].online = true;
                        app.attached.retain(|(v, _)| *v != i);
                        app.status = format!("{} connected", app.vms[i].name);
                        app.send(i, Req::List);
                        app.dirty = true;
                    }
                    Ui::Down(i) => {
                        app.vms[i].online = false;
                        app.status = format!("{} offline — retrying", app.vms[i].name);
                        app.dirty = true;
                    }
                    Ui::Msg(i, resp) => {
                        match resp {
                            Resp::Sessions { sessions: list } => {
                                app.vms[i].sessions = list;
                                app.rebuild();
                                app.reconcile(i);
                            }
                            Resp::Output { id, data, live } => {
                                let selected = app
                                    .cur()
                                    .is_some_and(|(vi, s)| vi == i && s.id == id);
                                if app.selection.as_ref().is_some_and(|selection| {
                                    selection.down.is_none()
                                        && selection.vm == i
                                        && selection.id == id
                                }) {
                                    app.selection = None;
                                }
                                let mut copy = None;
                                if let (Ok(bytes), Some(p)) =
                                    (unb64(&data), app.panes.get_mut(&(i, id)))
                                {
                                    p.process(&bytes);
                                    copy = p.callbacks_mut().take_if(live && selected).pop();
                                }
                                if let Some(copy) = copy {
                                    app.status = match copy_local(&copy) {
                                        Ok(()) => format!(
                                            "copied {} chars",
                                            String::from_utf8_lossy(&copy).chars().count()
                                        ),
                                        Err(e) => format!("copy failed: {e}"),
                                    };
                                }
                            }
                            Resp::Exited { id, code } => {
                                if let Some(s) = app.vms[i].sessions.iter_mut().find(|s| s.id == id) {
                                    s.status = Status::Stopped;
                                    app.status = format!("{} exited ({code}) — r to restart", s.name);
                                }
                                if app.focus == Focus::Terminal { app.focus = Focus::Sidebar }
                            }
                            Resp::Error { msg: e } => app.status = format!("{}: {e}", app.vms[i].name),
                        }
                        app.dirty = true;
                    }
                }
            }
        }
    };
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paste_is_one_message_not_one_per_line() {
        let out = paste_bytes("a\r\nb\nc\x1b[201~d", true);
        assert_eq!(out, b"\x1b[200~a\rb\rcd\x1b[201~".to_vec());
        assert_eq!(paste_bytes("a\nb", false), b"a\rb".to_vec());
    }

    #[test]
    fn osc52_copy_survives_split_pty_chunks() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, Clipboard::default());
        parser.process(b"\x1b]52;c;aGV");
        parser.process(b"sbG8=\x07");
        parser.process(b"\x1b]52;c;?\x07"); // reads never reach the local clipboard

        assert_eq!(parser.callbacks_mut().take(), vec![b"hello".to_vec()]);
        assert!(parser.callbacks_mut().take().is_empty());
    }

    #[test]
    fn suppressed_osc52_copies_are_discarded() {
        let mut parser =
            vt100::Parser::new_with_callbacks(24, 80, 0, Clipboard::default());
        parser.process(b"\x1b]52;c;c3RhbGU=\x07");

        assert!(parser.callbacks_mut().take_if(false).is_empty());
        assert!(parser.callbacks_mut().take_if(true).is_empty());
    }

    #[test]
    fn drag_selection_is_inclusive_and_direction_independent() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"alpha\r\nbeta");

        assert_eq!(selected_text(parser.screen(), (1, 0), (1, 1)), Some("lpha\nbe".into()));
        assert_eq!(selected_text(parser.screen(), (1, 1), (1, 0)), Some("lpha\nbe".into()));
        assert_eq!(selected_text(parser.screen(), (1, 0), (1, 0)), None);
    }

    #[test]
    fn stationary_left_drag_remains_a_child_click() {
        use ratatui::crossterm::event::MouseButton;
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1003h\x1b[?1006h");
        let event = |kind| MouseEvent {
            kind,
            column: 12,
            row: 8,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            click_bytes(
                event(MouseEventKind::Down(MouseButton::Left)),
                event(MouseEventKind::Up(MouseButton::Left)),
                parser.screen(),
                (2, 3),
            ),
            b"\x1b[<0;3;4M\x1b[<0;3;4m"
        );
    }

    #[test]
    fn drag_endpoints_clamp_to_the_visible_pane() {
        assert_eq!(pane_cell((10, 5), (20, 4), (12, 6), false), Some((2, 1)));
        assert_eq!(pane_cell((10, 5), (20, 4), (2, 99), false), None);
        assert_eq!(pane_cell((10, 5), (20, 4), (2, 99), true), Some((0, 3)));
    }

    #[test]
    fn mouse_bytes_match_a_real_terminal() {
        use ratatui::crossterm::event::MouseButton;
        let ev = |kind| MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE };
        let screen = |seq: &str| {
            let mut p = vt100::Parser::new(24, 80, 0);
            p.process(seq.as_bytes());
            p
        };

        // No mouse mode: the app gets nothing and we keep the event.
        let off = screen("");
        assert!(mouse_bytes(ev(MouseEventKind::ScrollUp), off.screen(), 0, 0).is_none());

        // What Claude Code asks for: any-motion tracking, SGR encoding.
        let sgr = screen("\x1b[?1003h\x1b[?1006h");
        let b = |kind, c, r| {
            mouse_bytes(ev(kind), sgr.screen(), c, r).map(|v| String::from_utf8(v).unwrap())
        };
        assert_eq!(b(MouseEventKind::ScrollUp, 4, 9).unwrap(), "\x1b[<64;5;10M");
        assert_eq!(b(MouseEventKind::ScrollDown, 0, 0).unwrap(), "\x1b[<65;1;1M");
        assert_eq!(b(MouseEventKind::Down(MouseButton::Left), 2, 3).unwrap(), "\x1b[<0;3;4M");
        assert_eq!(b(MouseEventKind::Up(MouseButton::Left), 2, 3).unwrap(), "\x1b[<0;3;4m");
        assert_eq!(b(MouseEventKind::Drag(MouseButton::Left), 2, 3).unwrap(), "\x1b[<32;3;4M");
        assert_eq!(b(MouseEventKind::Moved, 2, 3).unwrap(), "\x1b[<35;3;4M");

        // Press/release only (?1000h): drags and moves stay with us.
        let vt200 = screen("\x1b[?1000h\x1b[?1006h");
        assert!(mouse_bytes(ev(MouseEventKind::Moved), vt200.screen(), 1, 1).is_none());
        assert!(mouse_bytes(ev(MouseEventKind::Drag(MouseButton::Left)), vt200.screen(), 1, 1).is_none());
        assert!(mouse_bytes(ev(MouseEventKind::ScrollUp), vt200.screen(), 1, 1).is_some());

        // Legacy encoding: 0x20-biased bytes, release reported as button 3.
        let x10 = screen("\x1b[?1000h");
        assert_eq!(
            mouse_bytes(ev(MouseEventKind::Down(MouseButton::Right)), x10.screen(), 2, 3).unwrap(),
            vec![0x1b, b'[', b'M', 34, 35, 36]
        );
        assert_eq!(
            mouse_bytes(ev(MouseEventKind::Up(MouseButton::Right)), x10.screen(), 2, 3).unwrap(),
            vec![0x1b, b'[', b'M', 35, 35, 36]
        );
    }

    #[test]
    fn sidebar_clicks_land_on_the_right_row() {
        // The border columns of a 30-wide sidebar are 29 (its own) and 30 (the pane's).
        assert!(!on_split(28, 30));
        assert!(on_split(29, 30));
        assert!(on_split(30, 30));
        assert!(!on_split(31, 30));

        let rows = [Row::Vm(0), Row::Elide(1), Row::Session(0, 0, 2), Row::Folder(0)];
        assert_eq!(row_at(&rows, 0, 1, 1), Some(0)); // first line under the border
        assert_eq!(row_at(&rows, 0, 1, 3), Some(2));
        assert_eq!(row_at(&rows, 0, 1, 2), None); // "…" is inert
        assert_eq!(row_at(&rows, 0, 1, 0), None); // the border itself
        assert_eq!(row_at(&rows, 0, 1, 9), None); // empty space below the list
        assert_eq!(row_at(&rows, 2, 1, 1), Some(2)); // scrolled two rows down
    }

    #[test]
    fn default_name_is_the_folder() {
        assert_eq!(default_name("/Users/d/Documents/agents-hub", "claude"), "agents-hub");
        assert_eq!(default_name("/Users/d/Documents/agents-hub/", "claude"), "agents-hub");
        assert_eq!(default_name("~/src/foo", "claude"), "foo");
        // Nothing folder-shaped to use — fall back to the agent.
        assert_eq!(default_name("~", "claude"), "claude");
        assert_eq!(default_name("/", "claude"), "claude");
        assert_eq!(default_name("", "claude"), "claude");
    }

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
        let out = tree(&s, &[0, 1], &hidden);
        assert_eq!(
            shape(&out),
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

    #[test]
    fn key_bytes_match_a_real_terminal() {
        let plain = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
        let code = |c| KeyEvent::new(c, KeyModifiers::NONE);

        assert_eq!(key_bytes(plain('a')).unwrap(), b"a");
        assert_eq!(key_bytes(plain('é')).unwrap(), "é".as_bytes());
        assert_eq!(key_bytes(ctrl('c')).unwrap(), vec![3]);
        assert_eq!(key_bytes(ctrl('d')).unwrap(), vec![4]);
        assert_eq!(key_bytes(ctrl(' ')).unwrap(), vec![0]);
        // crossterm's legacy encoding for bytes 0x1c..=0x1f
        assert_eq!(key_bytes(ctrl('4')).unwrap(), vec![0x1c]); // Ctrl-\, SIGQUIT
        assert_eq!(key_bytes(ctrl('5')).unwrap(), vec![0x1d]);
        assert_eq!(key_bytes(ctrl('6')).unwrap(), vec![0x1e]);
        assert_eq!(key_bytes(ctrl('7')).unwrap(), vec![0x1f]);
        assert_eq!(key_bytes(ctrl('\\')).unwrap(), vec![0x1c]);
        assert_eq!(key_bytes(code(KeyCode::Enter)).unwrap(), b"\r");
        assert_eq!(key_bytes(code(KeyCode::Backspace)).unwrap(), vec![0x7f]);
        assert_eq!(key_bytes(code(KeyCode::Up)).unwrap(), b"\x1b[A");
        assert_eq!(key_bytes(code(KeyCode::F(1))).unwrap(), b"\x1bOP");
        assert_eq!(key_bytes(code(KeyCode::F(4))).unwrap(), b"\x1bOS");
        assert_eq!(key_bytes(code(KeyCode::F(5))).unwrap(), b"\x1b[15~");
        assert_eq!(key_bytes(code(KeyCode::F(12))).unwrap(), b"\x1b[24~");
        // Alt prefixes ESC, the way xterm sends meta.
        assert_eq!(
            key_bytes(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::ALT)).unwrap(),
            b"\x1bb"
        );
    }
}
