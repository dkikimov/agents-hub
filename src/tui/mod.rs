//! The client. One task per configured VM (local socket or `ssh host agents-hub stdio`),
//! a vt100 parser per session, and a sidebar tree grouped by VM name.
//!
//! `app` holds the state, `event` is the only thing that mutates it, `render` the only
//! thing that paints it; `link`, `tree`, `input` and `clipboard` are pure leaves.

mod app;
mod clipboard;
mod event;
mod input;
mod link;
mod render;
mod tree;

use crate::config::Config;
use crate::proto::{Req, Resp};
use anyhow::{bail, Result};
use app::{App, VmState};
use ratatui::crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    KeyEventKind,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// One redraw per frame at most, however many bytes arrive in between. This is what
/// keeps the UI responsive when three agents stream at once.
const FRAME: Duration = Duration::from_millis(16);
const ACTIVITY_WINDOW: Duration = Duration::from_secs(1);

/// Sidebar width: start, and the bounds the drag handle clamps to.
const SIDE_W: u16 = 30;
const SIDE_MIN: u16 = 14;
const PANE_MIN: u16 = 20;

/// Lines per wheel tick.
const WHEEL: usize = 3;

/// Directories the new-session cwd menu shows at once; the rest scroll past.
const CWD_MENU: usize = 6;

/// Everything the run loop can wake up for, from any of its sources.
enum Ui {
    Input(Event),
    Up(usize),
    Down(usize),
    Msg(usize, Resp),
}

/// crossterm's `read()` is blocking; a plain thread is the whole adapter.
fn spawn_key_reader(tx: UnboundedSender<Ui>) {
    std::thread::spawn(move || loop {
        match ratatui::crossterm::event::read() {
            Ok(ev) => {
                if tx.send(Ui::Input(ev)).is_err() {
                    return;
                }
            }
            Err(_) => return,
        }
    });
}

/// Mouse capture keeps the pane/sidebar mouse features available; pane drags are
/// copied locally. Bracketed paste is what keeps a multi-line paste one message:
/// without it the host sends each line as a separate Enter, and an agent prompt
/// submits on every one. Both are undone on the way out *and* from a panic hook.
fn enter_raw_extras() {
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        EnableMouseCapture,
        EnableBracketedPaste
    );
    let hook = std::panic::take_hook(); // ratatui's, which restores the screen
    std::panic::set_hook(Box::new(move |info| {
        leave_raw_extras();
        hook(info);
    }));
}

fn leave_raw_extras() {
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
}

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
        tokio::spawn(link::vm_task(i, vm.clone(), tx.clone(), req_rx));
    }
    spawn_key_reader(tx.clone());

    let mut app = App::new(vms, cfg.agent_names());
    let mut term = ratatui::init();
    enter_raw_extras();

    let mut ticker = tokio::time::interval(FRAME);
    let result = loop {
        tokio::select! {
            _ = ticker.tick() => {
                // The activity dots expire on their own, so a session that fell quiet
                // needs one more frame even with nothing else to report.
                let before = app.activity.len();
                let now = Instant::now();
                app.activity.retain(|_, last| now.duration_since(*last) < ACTIVITY_WINDOW);
                app.dirty |= app.activity.len() != before;
                if app.dirty {
                    app.dirty = false;
                    if let Err(e) = term.draw(|f| render::draw(f, &mut app)) { break Err(e.into()) }
                }
            }
            ev = rx.recv() => {
                let Some(ev) = ev else { break Ok(()) };
                match ev {
                    Ui::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                        if !event::on_key(&mut app, k) { break Ok(()) }
                    }
                    Ui::Input(Event::Mouse(m)) => event::on_mouse(&mut app, m),
                    Ui::Input(Event::Paste(text)) => event::on_paste(&mut app, &text),
                    Ui::Input(Event::Resize(..)) => {
                        app.selection = None;
                        app.dirty = true;
                    }
                    Ui::Input(_) => {}
                    Ui::Up(i) => {
                        app.vms[i].online = true;
                        // Force a re-attach: the daemon on the far side has no memory
                        // of the subscriptions the dropped connection held.
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
                    Ui::Msg(i, resp) => event::on_msg(&mut app, i, resp),
                }
            }
        }
    };
    leave_raw_extras();
    ratatui::restore();
    result
}
