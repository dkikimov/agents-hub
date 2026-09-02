//! Painting. Reads `App`, writes only the geometry it discovers on the way
//! (`pane_org`, `side_org`, `side_off`) back into it, because a click can't be
//! mapped to a cell without knowing where the widgets ended up.

use super::app::{default_name, App, Focus, Modal, Row};
use super::{ACTIVITY_WINDOW, PANE_MIN, SIDE_MIN};
use crate::proto::Status;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use std::time::Instant;
use tui_term::widget::PseudoTerminal;

fn session_marker(
    online: bool,
    status: Status,
    last_activity: Option<Instant>,
    now: Instant,
) -> (&'static str, Color) {
    match (online, status) {
        (false, _) => ("◌", Color::DarkGray),
        (true, Status::Stopped) => ("○", Color::DarkGray),
        (true, Status::Running)
            if last_activity.is_some_and(|last| now.duration_since(last) < ACTIVITY_WINDOW) =>
        {
            ("◉", Color::Yellow)
        }
        (true, Status::Running) => ("●", Color::Green),
    }
}

fn sidebar_items(app: &App) -> Vec<ListItem<'static>> {
    let now = Instant::now();
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
                let last = app.activity.get(&(vi, s.id.clone())).copied();
                let (glyph, color) = session_marker(vm.online, s.status, last, now);
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

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
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
                        .fg(if filter_active {
                            Color::Yellow
                        } else {
                            Color::Reset
                        })
                        .add_modifier(if filter_active {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                )),
        )
        .highlight_style(
            Style::default()
                .bg(if side_focused {
                    Color::Blue
                } else {
                    Color::DarkGray
                })
                .add_modifier(Modifier::BOLD),
        );
    let mut st = ListState::default().with_offset(app.side_off);
    st.select(Some(app.sel));
    f.render_stateful_widget(list, area, &mut st);
    // Rendering clamps the offset and scrolls to the selection; keep what it settled on
    // so clicks map to the rows actually on screen.
    app.side_off = st.offset();
    app.side_org = (area.x + 1, area.y + 1);
}

/// Paints the drag highlight over the cells it covers, in whichever direction it ran.
fn draw_selection(f: &mut Frame, app: &App, inner: Rect, vi: usize, id: &str) {
    let Some(sel) = app
        .selection
        .as_ref()
        .filter(|s| s.vm == vi && s.id == id && s.start != s.end)
    else {
        return;
    };
    let (start, end) = if (sel.start.1, sel.start.0) <= (sel.end.1, sel.end.0) {
        (sel.start, sel.end)
    } else {
        (sel.end, sel.start)
    };
    for row in start.1..=end.1 {
        let first = if row == start.1 { start.0 } else { 0 };
        let last = if row == end.1 {
            end.0
        } else {
            inner.width.saturating_sub(1)
        };
        for col in first..=last {
            f.buffer_mut()[(inner.x + col, inner.y + row)]
                .set_style(Style::default().add_modifier(Modifier::REVERSED));
        }
    }
}

fn draw_pane(f: &mut Frame, app: &mut App, area: Rect) {
    let sel = app.cur().map(|(vi, s)| (vi, s.clone()));
    let pane_title = match &sel {
        Some((vi, s)) => {
            let last = app.activity.get(&(*vi, s.id.clone())).copied();
            let (glyph, color) =
                session_marker(app.vms[*vi].online, s.status, last, Instant::now());
            Line::from(vec![
                Span::raw(" "),
                Span::styled(glyph, Style::default().fg(color)),
                Span::raw(format!(" {} · {} — {} ", s.agent, s.name, app.vms[*vi].name)),
            ])
        }
        None => Line::from(" no session selected "),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(match app.focus {
            Focus::Sidebar => Color::DarkGray,
            Focus::Terminal => Color::Cyan,
            Focus::Scrollback => Color::Yellow,
        }))
        .title(pane_title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    app.pane_org = (inner.x, inner.y);
    app.resize_panes(inner.width.max(1), inner.height.max(1));

    let Some((vi, s)) = sel else {
        f.render_widget(
            Paragraph::new("\n  Select a session, or press n to start one.")
                .style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    };
    let Some(parser) = app.panes.get(&(vi, s.id.clone())) else {
        f.render_widget(Paragraph::new("connecting…"), inner);
        return;
    };
    let screen = parser.screen();
    let cursor = (!screen.hide_cursor()).then(|| screen.cursor_position());
    f.render_widget(PseudoTerminal::new(screen), inner);
    draw_selection(f, app, inner, vi, &s.id);
    if let (Focus::Terminal, Some((r, c))) = (&app.focus, cursor) {
        f.set_cursor_position((inner.x + c, inner.y + r));
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
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

    draw_sidebar(f, app, cols[0]);
    draw_pane(f, app, cols[1]);
    draw_status(f, app, outer[1]);

    if let Some(m) = &app.modal {
        draw_modal(f, app, m);
    }
}

/// Hints always visible; status rides the right edge so it never hides them.
fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let live = app
        .cur()
        .is_some_and(|(vi, s)| s.status == Status::Running && app.vms[vi].online);
    let hint = match (&app.focus, live) {
        (Focus::Scrollback, _) => {
            "  SCROLLBACK · j/k ↑↓ line · PgUp/PgDn page · g/G ends · q/esc live"
        }
        // Never let keystrokes vanish into a dead pane without saying so.
        (Focus::Terminal, false) => {
            "  ^] back to list · session is not running — press r to restart"
        }
        (Focus::Terminal, true) => "  ^] back to list · keys go to the session",
        (Focus::Sidebar, _) => {
            "  [ scrollback  n new  d kill  r restart  ⏎ attach  / filter  ? help  q quit"
        }
    };
    let bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(10), Constraint::Length(34)])
        .split(area);
    let alternate = app.focus == Focus::Scrollback
        && app
            .cur()
            .and_then(|(vi, s)| app.panes.get(&(vi, s.id.clone())))
            .is_some_and(|p| p.screen().alternate_screen());
    let status = if alternate {
        "full-screen app owns scrolling"
    } else {
        &app.status
    };
    f.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        bar[0],
    );
    f.render_widget(
        Paragraph::new(status)
            .right_aligned()
            .style(Style::default().fg(Color::DarkGray)),
        bar[1],
    );
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
                Line::from("  ^] then [ local scrollback; q / esc returns live"),
                Line::from("  /        filter          q   quit"),
                Line::from("  space    fold a folder's middle away into …"),
                Line::from("  mouse    click a row to switch · drag the split to resize"),
                Line::from("           click pane controls · drag pane text to copy"),
                Line::from("           click a link in the pane to open it"),
                Line::from(""),
                Line::from(Span::styled(
                    "  folders come from each session's cwd; n starts one there",
                    Style::default().fg(Color::DarkGray),
                )),
            ],
            62,
            13,
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

#[cfg(test)]
mod tests {
    use super::super::app::fixture::app;
    use super::*;
    use std::time::Duration;

    #[test]
    fn session_marker_reflects_lifecycle_and_recent_output() {
        let now = Instant::now();
        let recent = Some(now - Duration::from_millis(999));
        let quiet = Some(now - Duration::from_secs(1));

        assert_eq!(
            session_marker(false, Status::Running, recent, now),
            ("◌", Color::DarkGray)
        );
        assert_eq!(
            session_marker(true, Status::Stopped, recent, now),
            ("○", Color::DarkGray)
        );
        assert_eq!(
            session_marker(true, Status::Running, None, now),
            ("●", Color::Green)
        );
        assert_eq!(
            session_marker(true, Status::Running, recent, now),
            ("◉", Color::Yellow)
        );
        assert_eq!(
            session_marker(true, Status::Running, quiet, now),
            ("●", Color::Green)
        );
    }

    /// The one test that actually paints: a narrow terminal must still leave both
    /// halves usable, and the pane geometry it reports back is what clicks are
    /// resolved against.
    #[test]
    fn a_cramped_terminal_still_yields_a_usable_split() {
        let (mut a, _rx) = app(&["~/work/api"]);
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 12)).unwrap();
        term.draw(|f| draw(f, &mut a)).unwrap();

        assert_eq!(a.side_w, 40 - PANE_MIN);
        assert_eq!(a.side_org, (1, 1));
        assert_eq!(a.pane_org, (a.side_w + 1, 1));
        assert_eq!(a.pane, (PANE_MIN - 2, 12 - 1 - 2));

        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("local"), "the VM group is always listed");
        assert!(text.contains("api"), "and the session under it");
    }

    #[test]
    fn scrollback_mode_is_visible_while_guest_input_is_paused() {
        let (mut a, _rx) = app(&["~/work/api"]);
        a.reconcile(0);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Scrollback;
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();

        term.draw(|f| draw(f, &mut a)).unwrap();

        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("SCROLLBACK"));
    }

    #[test]
    fn alternate_screen_notice_tracks_the_live_parser_state() {
        let (mut a, _rx) = app(&["~/work/api"]);
        a.reconcile(0);
        a.sel = a.rows.len() - 1;
        a.focus = Focus::Scrollback;
        let pane = (0, "s0".to_string());
        a.panes.get_mut(&pane).unwrap().process(b"primary\r\n");
        a.panes.get_mut(&pane).unwrap().process(b"\x1b[?1049h");
        let mut term =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 12)).unwrap();

        term.draw(|f| draw(f, &mut a)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("full-screen app owns scrolling"));

        a.panes.get_mut(&pane).unwrap().process(b"\x1b[?1049l");
        term.draw(|f| draw(f, &mut a)).unwrap();
        let text: String = term
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(!text.contains("full-screen app owns scrolling"));
    }
}
