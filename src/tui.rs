//! The client. One task per configured VM (local socket or `ssh host agents-hub stdio`),
//! a vt100 parser per session, and a sidebar tree grouped by VM name.

use crate::config::{Config, Vm};
use crate::proto::*;
use anyhow::{bail, Result};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tui_term::widget::PseudoTerminal;
use tui_term::vt100;

/// One redraw per frame at most, however many bytes arrive in between. This is what
/// keeps the UI responsive when three agents stream at once.
const FRAME: Duration = Duration::from_millis(16);

type Rd = Box<dyn AsyncRead + Unpin + Send>;
type Wr = Box<dyn AsyncWrite + Unpin + Send>;

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

// ── state ─────────────────────────────────────────────────────────────────────

struct VmState {
    name: String,
    local: bool,
    online: bool,
    sessions: Vec<SessionInfo>,
    tx: UnboundedSender<Req>,
}

#[derive(Clone, Copy)]
enum Row {
    Vm(usize),
    Session(usize, usize),
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

struct App {
    vms: Vec<VmState>,
    rows: Vec<Row>,
    sel: usize,
    focus: Focus,
    panes: HashMap<(usize, String), vt100::Parser>,
    attached: HashSet<(usize, String)>,
    modal: Option<Modal>,
    filter: String,
    editing_filter: bool,
    agents: Vec<String>,
    pane: (u16, u16), // cols, rows
    status: String,
    dirty: bool,
}

impl App {
    fn rebuild(&mut self) {
        let mut rows = std::mem::take(&mut self.rows);
        rows.clear();
        let f = self.filter.to_lowercase();
        for (vi, vm) in self.vms.iter().enumerate() {
            rows.push(Row::Vm(vi));
            for (si, s) in vm.sessions.iter().enumerate() {
                let hit = f.is_empty()
                    || s.name.to_lowercase().contains(&f)
                    || s.agent.to_lowercase().contains(&f);
                if hit {
                    rows.push(Row::Session(vi, si));
                }
            }
        }
        self.rows = rows;
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
    }

    fn cur(&self) -> Option<(usize, &SessionInfo)> {
        match self.rows.get(self.sel)? {
            Row::Session(vi, si) => Some((*vi, self.vms[*vi].sessions.get(*si)?)),
            Row::Vm(_) => None,
        }
    }

    fn cur_vm(&self) -> usize {
        match self.rows.get(self.sel) {
            Some(Row::Vm(vi)) | Some(Row::Session(vi, _)) => *vi,
            None => 0,
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

        let fresh: Vec<String> = live
            .iter()
            .filter(|id| !self.attached.contains(&(vi, (*id).clone())))
            .cloned()
            .collect();
        for id in fresh {
            self.panes
                .insert((vi, id.clone()), vt100::Parser::new(rows, cols, 2000));
            self.attached.insert((vi, id.clone()));
            self.send(vi, Req::Attach { id, cols, rows });
        }
    }

    fn resize_panes(&mut self, cols: u16, rows: u16) {
        if self.pane == (cols, rows) {
            return;
        }
        self.pane = (cols, rows);
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

/// Returns false when the app should quit.
fn on_key(app: &mut App, k: KeyEvent) -> bool {
    app.dirty = true;

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
                    app.send(vi, Req::Input { id, data: b64(&bytes) });
                }
            }
        }
        return true;
    }

    match k.code {
        KeyCode::Char('q') => return false,
        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return false,
        KeyCode::Char('j') | KeyCode::Down => {
            app.sel = (app.sel + 1).min(app.rows.len().saturating_sub(1))
        }
        KeyCode::Char('k') | KeyCode::Up => app.sel = app.sel.saturating_sub(1),
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
        KeyCode::Char('n') => {
            let vm = app.cur_vm();
            let cwd = if app.vms[vm].local {
                std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|_| "~".into())
            } else {
                "~".into()
            };
            app.modal = Some(Modal::New {
                vm,
                agent: 0,
                name: String::new(),
                cwd,
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
                            agent_name.clone()
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
            Row::Session(vi, si) => {
                let vm = &app.vms[vi];
                let s = &vm.sessions[si];
                let (glyph, color) = match (vm.online, s.status) {
                    (false, _) => ("◌", Color::DarkGray),
                    (true, Status::Running) => ("●", Color::Green),
                    (true, Status::Stopped) => ("○", Color::DarkGray),
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  "),
                    Span::styled(glyph, Style::default().fg(color)),
                    Span::raw(" "),
                    Span::styled(
                        format!("{:<7}", s.agent),
                        Style::default().fg(Color::Magenta),
                    ),
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
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(30), Constraint::Min(20)])
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
    let mut st = ListState::default();
    st.select(Some(app.sel));
    f.render_stateful_widget(list, cols[0], &mut st);

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

    app.resize_panes(inner.width.max(1), inner.height.max(1));

    match sel {
        Some((vi, s)) => match app.panes.get(&(vi, s.id.clone())) {
            Some(parser) => {
                let screen = parser.screen();
                f.render_widget(PseudoTerminal::new(screen), inner);
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
                Line::from(""),
                Line::from(Span::styled(
                    "  VM groups come from [[vm]] in config.toml",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            56,
            9,
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
                    Line::from(format!("{} name   {}", mark(1), name)),
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
        sel: 0,
        focus: Focus::Sidebar,
        panes: HashMap::new(),
        attached: HashSet::new(),
        modal: None,
        filter: String::new(),
        editing_filter: false,
        agents: cfg.agent_names(),
        pane: (80, 24),
        status: String::new(),
        dirty: true,
    };
    app.rebuild();

    let mut term = ratatui::init();
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
                    Ui::Input(Event::Resize(..)) => app.dirty = true,
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
                            Resp::Output { id, data } => {
                                if let (Ok(bytes), Some(p)) =
                                    (unb64(&data), app.panes.get_mut(&(i, id)))
                                {
                                    p.process(&bytes);
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
    ratatui::restore();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

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
