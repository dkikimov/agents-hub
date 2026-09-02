//! Every mutation of `App`: keys, mouse, paste, and frames arriving from a daemon.
//! Handlers only ever read pure helpers out of `input` and `tree`, which is what
//! makes them testable without a terminal.

use super::app::{default_name, App, Focus, Modal, Row, Selection};
use super::clipboard::copy_local;
use super::input::{
    click_bytes, key_bytes, mouse_bytes, pane_cell, paste_bytes, row_at, on_split, scroll_screen,
    selected_text, url_at,
};
use super::WHEEL;
use crate::proto::{b64, unb64, Req, Resp, Status};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use std::time::Instant;

// ── keys ──────────────────────────────────────────────────────────────────────

fn leave_scrollback(app: &mut App, focus: Focus) {
    if app.focus != Focus::Scrollback {
        return;
    }
    if let Some((vi, id, _)) = app.cur_live() {
        if let Some(p) = app.panes.get_mut(&(vi, id)) {
            p.screen_mut().set_scrollback(0);
        }
    }
    app.focus = focus;
}

/// Returns false when the app should quit.
pub fn on_key(app: &mut App, k: KeyEvent) -> bool {
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

    if app.focus == Focus::Scrollback {
        if matches!(k.code, KeyCode::Char('q') | KeyCode::Esc) {
            leave_scrollback(app, Focus::Terminal);
            return true;
        }
        let Some((vi, id, _)) = app.cur_live() else {
            return true;
        };
        let rows = usize::from(app.pane.1);
        if let Some(p) = app.panes.get_mut(&(vi, id)) {
            match k.code {
                KeyCode::Char('k') | KeyCode::Up => scroll_screen(p.screen_mut(), true, 1),
                KeyCode::Char('j') | KeyCode::Down => scroll_screen(p.screen_mut(), false, 1),
                KeyCode::PageUp => scroll_screen(p.screen_mut(), true, rows),
                KeyCode::PageDown => scroll_screen(p.screen_mut(), false, rows),
                KeyCode::Char('g') | KeyCode::Home => {
                    scroll_screen(p.screen_mut(), true, usize::MAX)
                }
                KeyCode::Char('G') | KeyCode::End => {
                    scroll_screen(p.screen_mut(), false, usize::MAX)
                }
                _ => {}
            }
        }
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
        if let Some((vi, id, true)) = app.cur_live() {
            if let Some(bytes) = key_bytes(k) {
                // Typing jumps back to the live bottom, like every other terminal.
                if let Some(p) = app.panes.get_mut(&(vi, id.clone())) {
                    p.screen_mut().set_scrollback(0);
                }
                app.send(vi, Req::Input { id, data: b64(&bytes) });
            }
        }
        return true;
    }

    match k.code {
        KeyCode::Char('[') => {
            if let Some((vi, id, _)) = app.cur_live() {
                if app.panes.contains_key(&(vi, id)) {
                    app.focus = Focus::Scrollback;
                }
            }
        }
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
            if let Some((vi, id, false)) = app.cur_live() {
                let (cols, rows) = app.pane;
                app.send(vi, Req::Restart { id, cols, rows });
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
                    agent = agent
                        .checked_sub(1)
                        .unwrap_or(app.agents.len().saturating_sub(1))
                }
                KeyCode::Right if field == 0 => {
                    agent = if app.agents.is_empty() {
                        0
                    } else {
                        (agent + 1) % app.agents.len()
                    }
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
                app.modal = Some(Modal::New {
                    vm,
                    agent,
                    name,
                    cwd,
                    field,
                });
            }
        }
    }
    true
}

// ── paste ─────────────────────────────────────────────────────────────────────

pub fn on_paste(app: &mut App, text: &str) {
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
    let Some((vi, id, true)) = app.cur_live() else {
        return;
    };
    let Some(p) = app.panes.get_mut(&(vi, id.clone())) else {
        return;
    };
    let bracketed = p.screen().bracketed_paste();
    p.screen_mut().set_scrollback(0);
    // ponytail: sent as one frame, and the daemon's write_all to the pty is blocking — a
    // multi-MB paste stalls that client's connection task. Chunk it if that ever bites.
    let data = b64(&paste_bytes(text, bracketed));
    app.send(vi, Req::Input { id, data });
}

// ── mouse ─────────────────────────────────────────────────────────────────────

/// Finishes a drag over the pane: copy what it covered, or — if it never moved —
/// replay it to the guest as a click.
fn finish_drag(app: &mut App, m: MouseEvent, left_up: bool) {
    let Some(cell) = pane_cell(app.pane_org, app.pane, (m.column, m.row), true) else {
        return;
    };
    if !left_up {
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
        app.status = copy_status(&text.clone().into_bytes());
        selection.down = None;
        app.selection = Some(selection);
    } else if let Some(url) = app
        .panes
        .get(&(selection.vm, selection.id.clone()))
        .and_then(|p| url_at(p.screen(), selection.start))
    {
        // The host terminal's own cmd-click can't see this pane: mouse capture takes the
        // click first, and cmd isn't encodable in any mouse protocol, so we open it.
        open_link(&url);
        app.status = "opened link".into();
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
        if running && app.focus != Focus::Scrollback {
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
}

/// Mouse over the sidebar selects and scrolls; the border between the two panes drags to
/// resize. Over the pane, the app inside gets the event if it turned mouse reporting on
/// (Claude Code does — that's how its own chat scrolls); otherwise the wheel walks our
/// vt100 scrollback instead.
pub fn on_mouse(app: &mut App, m: MouseEvent) {
    if app.modal.is_some() {
        return;
    }
    let left_down = matches!(m.kind, MouseEventKind::Down(MouseButton::Left));
    let left_drag = matches!(m.kind, MouseEventKind::Drag(MouseButton::Left));
    let left_up = matches!(m.kind, MouseEventKind::Up(MouseButton::Left));
    let wheel = match m.kind {
        MouseEventKind::ScrollUp => Some(true),
        MouseEventKind::ScrollDown => Some(false),
        _ => None,
    };

    if app.selection.as_ref().is_some_and(|s| s.down.is_some()) && (left_drag || left_up) {
        finish_drag(app, m, left_up);
        return;
    }

    if app.drag_split {
        match m.kind {
            MouseEventKind::Drag(_) => {
                app.side_w = (m.column + 1).max(super::SIDE_MIN); // draw clamps the far edge
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
            leave_scrollback(app, Focus::Sidebar);
            app.dirty = true;
            app.step(!up);
        }
        if let Some(i) = left_down
            .then(|| row_at(&app.rows, app.side_off, app.side_org.1, m.row))
            .flatten()
        {
            leave_scrollback(app, Focus::Sidebar);
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

    let Some((col, row)) = pane_cell(app.pane_org, app.pane, (m.column, m.row), false) else {
        return; // on the border or the status bar
    };
    let Some((vi, id, running)) = app.cur_live() else {
        return;
    };

    if left_down && m.modifiers == KeyModifiers::NONE && app.panes.contains_key(&(vi, id.clone())) {
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

    if app.focus == Focus::Scrollback {
        if let (Some(up), Some(p)) = (wheel, app.panes.get_mut(&(vi, id))) {
            scroll_screen(p.screen_mut(), up, WHEEL);
            app.dirty = true;
        }
        return;
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
        scroll_screen(p.screen_mut(), up, WHEEL);
    }
}

// ── frames from a daemon ──────────────────────────────────────────────────────

/// Off the UI thread: `open` waits for a cold browser to finish launching, and reaping the
/// child here is what keeps a session's worth of clicks from piling up as zombies.
fn open_link(url: &str) {
    if cfg!(test) {
        return; // no browser windows out of `cargo test`
    }
    let url = url.to_string();
    std::thread::spawn(move || {
        // Silenced: a scheme with no handler makes `open` write to the terminal we are
        // painting, which lands as garbage in the middle of the pane.
        std::process::Command::new("/usr/bin/open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    });
}

fn copy_status(data: &[u8]) -> String {
    match copy_local(data) {
        Ok(()) => format!("copied {} chars", String::from_utf8_lossy(data).chars().count()),
        Err(e) => format!("copy failed: {e}"),
    }
}

pub fn on_msg(app: &mut App, vi: usize, resp: Resp) {
    match resp {
        Resp::Sessions { sessions } => {
            app.vms[vi].sessions = sessions;
            app.rebuild();
            app.reconcile(vi);
        }
        Resp::Output { id, data, live } => {
            let selected = app.cur().is_some_and(|(v, s)| v == vi && s.id == id);
            // Output under a finished selection reflows the text it named; drop it
            // rather than highlight whatever moved into those cells.
            if app
                .selection
                .as_ref()
                .is_some_and(|s| s.down.is_none() && s.vm == vi && s.id == id)
            {
                app.selection = None;
            }
            let mut copy = None;
            if let (Ok(bytes), Some(p)) = (unb64(&data), app.panes.get_mut(&(vi, id.clone()))) {
                p.process(&bytes);
                copy = p.callbacks_mut().take_if(live && selected).pop();
                if live {
                    app.activity.insert((vi, id.clone()), Instant::now());
                }
            }
            if let Some(copy) = copy {
                app.status = copy_status(&copy);
            }
        }
        Resp::Exited { id, code } => {
            app.activity.remove(&(vi, id.clone()));
            if let Some(s) = app.vms[vi].sessions.iter_mut().find(|s| s.id == id) {
                s.status = Status::Stopped;
                app.status = format!("{} exited ({code}) — r to restart", s.name);
            }
            if app.focus == Focus::Terminal {
                app.focus = Focus::Sidebar;
            }
        }
        Resp::Error { msg } => app.status = format!("{}: {msg}", app.vms[vi].name),
    }
    app.dirty = true;
}

#[cfg(test)]
mod tests {
    use super::super::app::fixture::{app, sent, shape};
    use super::*;

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    #[test]
    fn q_quits_from_the_sidebar_but_types_into_a_session() {
        let (mut a, mut rx) = app(&["~/a"]);
        assert!(!on_key(&mut a, key('q')));

        a.focus = Focus::Terminal;
        a.sel = a.rows.len() - 1;
        assert!(on_key(&mut a, key('q')));
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"q")
            }]
        );
    }

    #[test]
    fn ctrl_bracket_is_the_way_back_out_in_both_spellings() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.sel = a.rows.len() - 1;
        for c in [']', '5'] {
            a.focus = Focus::Terminal;
            on_key(&mut a, ctrl(c));
            assert!(a.focus == Focus::Sidebar, "ctrl-{c} should leave the pane");
        }
        assert!(sent(&mut rx).is_empty(), "the escape key never reaches the guest");
    }

    #[test]
    fn ctrl_bracket_then_open_bracket_enters_local_scrollback() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Terminal;
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }

        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));
        on_key(&mut a, key('k'));

        assert_eq!(a.panes[&pane].screen().scrollback(), 1);
        assert!(sent(&mut rx).is_empty(), "scrollback keys stay local");
    }

    #[test]
    fn scrollback_keys_navigate_and_q_returns_to_the_live_guest() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }
        a.focus = Focus::Terminal;
        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));

        on_key(&mut a, code(KeyCode::PageUp));
        assert_eq!(a.panes[&pane].screen().scrollback(), 24);
        on_key(&mut a, code(KeyCode::PageDown));
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
        on_key(&mut a, key('g'));
        assert_eq!(a.panes[&pane].screen().scrollback(), 37);
        on_key(&mut a, key('G'));
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);

        on_key(&mut a, key('k'));
        on_key(&mut a, key('q'));
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
        on_key(&mut a, key('x'));
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"x")
            }]
        );
    }

    #[test]
    fn alternate_screen_keeps_primary_history_out_of_local_scrollback() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }
        a.panes.get_mut(&pane).unwrap().process(b"\x1b[?1049h");
        a.focus = Focus::Terminal;

        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));
        on_key(&mut a, key('g'));

        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
        assert!(a.status.is_empty());
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn keys_never_reach_a_stopped_session() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Terminal;
        a.vms[0].sessions[0].status = Status::Stopped;
        on_key(&mut a, key('x'));
        assert!(sent(&mut rx).is_empty());

        // ...but r restarts it, and only when it is actually stopped.
        a.focus = Focus::Sidebar;
        on_key(&mut a, key('r'));
        a.vms[0].sessions[0].status = Status::Running;
        on_key(&mut a, key('r'));
        assert_eq!(
            sent(&mut rx),
            [Req::Restart {
                id: "s0".into(),
                cols: 80,
                rows: 24
            }]
        );
    }

    #[test]
    fn filter_narrows_the_tree_and_esc_puts_it_back() {
        let (mut a, _rx) = app(&["~/work/api", "~/play/game"]);
        let full = shape(&a);

        on_key(&mut a, key('/'));
        for c in "game".chars() {
            on_key(&mut a, key(c));
        }
        assert_eq!(a.filter, "game");
        assert_eq!(shape(&a), ["local", "~/", "  play/", "    game/", "      game"]);

        on_key(&mut a, code(KeyCode::Backspace));
        assert_eq!(a.filter, "gam");
        on_key(&mut a, code(KeyCode::Esc));
        assert!(!a.editing_filter && a.filter.is_empty());
        assert_eq!(shape(&a), full);
    }

    #[test]
    fn space_folds_a_folder_and_leaves_the_selection_put() {
        let (mut a, _rx) = app(&["~/a/b/c"]);
        a.sel = 2; // "a/"
        on_key(&mut a, key(' '));
        assert_eq!(a.sel, 2);
        assert_eq!(shape(&a), ["local", "~/", "  a/", "    …", "      c/", "        c"]);
        on_key(&mut a, key(' '));
        assert_eq!(shape(&a), ["local", "~/", "  a/", "    b/", "      c/", "        c"]);

        // A leaf has nothing to fold.
        a.sel = 4; // "c/"
        on_key(&mut a, key(' '));
        assert!(a.collapsed.is_empty());
    }

    #[test]
    fn the_new_session_modal_seeds_from_the_selection_and_creates_on_enter() {
        let (mut a, mut rx) = app(&["~/work/api"]);
        a.sel = 2; // the "work/" folder
        on_key(&mut a, key('n'));
        // Name left blank, agent stepped to the second one.
        on_key(&mut a, code(KeyCode::Right));
        on_key(&mut a, code(KeyCode::Enter));
        assert_eq!(
            sent(&mut rx),
            [Req::Create {
                agent: "codex".into(),
                name: "work".into(),
                cwd: "~/work".into(),
                cols: 80,
                rows: 24
            }]
        );
        assert!(a.modal.is_none());
    }

    #[test]
    fn the_new_session_modal_types_into_the_focused_field_only() {
        let (mut a, mut rx) = app(&["~/work/api"]);
        a.sel = 2; // the "work/" folder
        on_key(&mut a, key('n'));
        on_key(&mut a, code(KeyCode::Tab)); // → name
        for c in "api".chars() {
            on_key(&mut a, key(c));
        }
        on_key(&mut a, code(KeyCode::Tab)); // → cwd
        on_key(&mut a, code(KeyCode::Backspace));
        on_key(&mut a, code(KeyCode::Enter));
        let Some(Req::Create { name, cwd, .. }) = sent(&mut rx).pop() else {
            panic!("expected a Create")
        };
        assert_eq!((name.as_str(), cwd.as_str()), ("api", "~/wor"));

        // Esc throws the whole thing away.
        on_key(&mut a, key('n'));
        on_key(&mut a, code(KeyCode::Esc));
        assert!(a.modal.is_none() && sent(&mut rx).is_empty());
    }

    #[test]
    fn killing_a_session_takes_a_confirmation() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.sel = a.rows.len() - 1;
        on_key(&mut a, key('d'));
        on_key(&mut a, key('x')); // anything else keeps asking
        assert!(a.modal.is_some());
        on_key(&mut a, key('n'));
        assert!(a.modal.is_none() && sent(&mut rx).is_empty());

        on_key(&mut a, key('d'));
        on_key(&mut a, key('y'));
        assert_eq!(sent(&mut rx), [Req::Kill { id: "s0".into() }]);
    }

    #[test]
    fn a_paste_into_the_sidebar_is_dropped_rather_than_run_as_commands() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.sel = a.rows.len() - 1;
        on_paste(&mut a, "q\nd\n");
        assert!(sent(&mut rx).is_empty());

        // In a text field it accumulates instead of being sent anywhere.
        on_key(&mut a, key('/'));
        on_paste(&mut a, "a\nb");
        assert_eq!(a.filter, "ab");
        assert!(sent(&mut rx).is_empty());
    }

    #[test]
    fn a_paste_reaches_the_guest_as_one_bracketed_frame() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Terminal;

        on_paste(&mut a, "one\ntwo");
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"one\rtwo")
            }]
        );

        // Once the guest asks for bracketed paste it gets the markers.
        a.panes
            .get_mut(&(0, "s0".to_string()))
            .unwrap()
            .process(b"\x1b[?2004h");
        on_paste(&mut a, "one\ntwo");
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"\x1b[200~one\rtwo\x1b[201~")
            }]
        );
    }

    #[test]
    fn a_sidebar_click_selects_and_the_split_starts_a_drag() {
        let (mut a, _rx) = app(&["~/a"]);
        a.side_org = (1, 1);
        let at = |column, row, kind| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let down = MouseEventKind::Down(MouseButton::Left);

        on_mouse(&mut a, at(3, 4, down)); // fourth list line → the session row
        assert_eq!(a.sel, 3);
        assert!(a.focus == Focus::Sidebar);

        // On the border it grabs the split instead, and the drag moves it.
        let border = a.side_w;
        on_mouse(&mut a, at(border, 4, down));
        assert!(a.drag_split);
        on_mouse(&mut a, at(20, 4, MouseEventKind::Drag(MouseButton::Left)));
        assert_eq!(a.side_w, 21);
        on_mouse(&mut a, at(20, 4, MouseEventKind::Up(MouseButton::Left)));
        assert!(!a.drag_split);
    }

    #[test]
    fn the_wheel_walks_our_scrollback_when_the_guest_ignores_the_mouse() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        a.pane_org = (31, 1);
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes.get_mut(&pane).unwrap().process(format!("line {i}\r\n").as_bytes());
        }

        let wheel = |kind| MouseEvent {
            kind,
            column: 40,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };
        on_mouse(&mut a, wheel(MouseEventKind::ScrollUp));
        assert_eq!(a.panes[&pane].screen().scrollback(), WHEEL);
        on_mouse(&mut a, wheel(MouseEventKind::ScrollDown));
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
        assert!(sent(&mut rx).is_empty(), "the guest never asked for the mouse");

        // Once it does, the wheel is its business and our scrollback stays put.
        a.panes.get_mut(&pane).unwrap().process(b"\x1b[?1000h\x1b[?1006h");
        on_mouse(&mut a, wheel(MouseEventKind::ScrollUp));
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"\x1b[<64;10;5M")
            }]
        );
    }

    #[test]
    fn clicking_a_link_opens_it_and_anything_else_still_reaches_the_guest() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        a.pane_org = (31, 1);
        let pane = (0, "s0".to_string());
        a.panes
            .get_mut(&pane)
            .unwrap()
            .process(b"see https://x.dev/p done\x1b[?1000h\x1b[?1006h");
        let click = |column| {
            [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ]
            .map(|kind| MouseEvent {
                kind,
                column,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
        };

        for m in click(39) {
            on_mouse(&mut a, m);
        }
        assert_eq!(a.status, "opened link");
        assert!(sent(&mut rx).is_empty(), "a link click never reaches the guest");

        for m in click(32) {
            on_mouse(&mut a, m);
        }
        assert_eq!(
            sent(&mut rx),
            [Req::Input {
                id: "s0".into(),
                data: b64(b"\x1b[<0;2;1M\x1b[<0;2;1m")
            }]
        );
    }

    #[test]
    fn scrollback_mode_keeps_mouse_local_when_the_guest_requested_it() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);
        a.sel = a.rows.len() - 1;
        a.pane_org = (31, 1);
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }
        a.panes
            .get_mut(&pane)
            .unwrap()
            .process(b"\x1b[?1000h\x1b[?1006h");
        a.focus = Focus::Terminal;
        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));
        let mouse = |kind| MouseEvent {
            kind,
            column: 40,
            row: 5,
            modifiers: KeyModifiers::NONE,
        };

        on_mouse(&mut a, mouse(MouseEventKind::ScrollUp));
        on_mouse(
            &mut a,
            mouse(MouseEventKind::Down(MouseButton::Right)),
        );

        assert_eq!(a.panes[&pane].screen().scrollback(), WHEEL);
        assert!(sent(&mut rx).is_empty(), "scrollback mode pauses guest mouse input");
    }

    #[test]
    fn clicking_the_sidebar_leaves_scrollback_and_resets_the_old_pane() {
        let (mut a, mut rx) = app(&["~/a", "~/b"]);
        a.reconcile(0);
        sent(&mut rx);
        a.side_org = (1, 1);
        let first = a
            .rows
            .iter()
            .position(|row| matches!(row, Row::Session(0, 0, _)))
            .unwrap();
        let second = a
            .rows
            .iter()
            .position(|row| matches!(row, Row::Session(0, 1, _)))
            .unwrap();
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }
        a.sel = first;
        a.focus = Focus::Terminal;
        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));
        on_key(&mut a, key('k'));
        let second_row = a.side_org.1 + second as u16;

        on_mouse(
            &mut a,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 3,
                row: second_row,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert_eq!(a.sel, second);
        assert!(a.focus == Focus::Sidebar);
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
    }

    #[test]
    fn wheeling_the_sidebar_leaves_scrollback_and_resets_the_old_pane() {
        let (mut a, mut rx) = app(&["~/a", "~/b"]);
        a.reconcile(0);
        sent(&mut rx);
        let first = a
            .rows
            .iter()
            .position(|row| matches!(row, Row::Session(0, 0, _)))
            .unwrap();
        let pane = (0, "s0".to_string());
        for i in 0..60 {
            a.panes
                .get_mut(&pane)
                .unwrap()
                .process(format!("line {i}\r\n").as_bytes());
        }
        a.sel = first;
        a.focus = Focus::Terminal;
        on_key(&mut a, ctrl(']'));
        on_key(&mut a, key('['));
        on_key(&mut a, key('k'));

        on_mouse(
            &mut a,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 3,
                row: 4,
                modifiers: KeyModifiers::NONE,
            },
        );

        assert!(a.focus == Focus::Sidebar);
        assert_eq!(a.panes[&pane].screen().scrollback(), 0);
    }

    #[test]
    fn an_exit_frame_stops_the_session_and_hands_focus_back() {
        let (mut a, _rx) = app(&["~/a"]);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Terminal;
        a.activity.insert((0, "s0".into()), Instant::now());

        on_msg(
            &mut a,
            0,
            Resp::Exited {
                id: "s0".into(),
                code: 1,
            },
        );
        assert_eq!(a.vms[0].sessions[0].status, Status::Stopped);
        assert!(a.focus == Focus::Sidebar);
        assert!(a.activity.is_empty());
        assert!(a.status.contains("exited (1)"));
    }

    #[test]
    fn output_feeds_the_pane_and_only_live_bytes_count_as_activity() {
        let (mut a, mut rx) = app(&["~/a"]);
        a.reconcile(0);
        sent(&mut rx);

        on_msg(
            &mut a,
            0,
            Resp::Output {
                id: "s0".into(),
                data: b64(b"replayed"),
                live: false,
            },
        );
        assert!(a.activity.is_empty(), "replay is history, not activity");

        on_msg(
            &mut a,
            0,
            Resp::Output {
                id: "s0".into(),
                data: b64(b" now"),
                live: true,
            },
        );
        assert!(a.activity.contains_key(&(0, "s0".to_string())));
        assert!(a.panes[&(0, "s0".to_string())]
            .screen()
            .contents()
            .starts_with("replayed now"));
    }
}
