//! What an agent is doing, read off its screen: working, waiting on you, or idle.
//!
//! Output timing can't tell these apart. A spinner repaints while a turn is quiet, a
//! long tool call prints nothing, and our own resize makes an idle agent redraw. What
//! the agent *shows* is unambiguous, though: Claude puts `esc to interrupt` under a
//! running turn, `Do you want to proceed?` over a permission prompt, a bare `❯` in an
//! idle prompt box, and a spinner at the front of its window title.
//!
//! The rules are herdr's Claude and Codex manifests (github.com/herdrdev/herdr,
//! Apache-2.0, `src/detect/manifests/{claude,codex}.toml`), ported rule for rule and
//! listed in its priority order, so the first rule that matches decides. Each carries
//! herdr's id so the two can be diffed when an agent's UI moves.
//!
//! ponytail: hand-written matchers instead of herdr's TOML + regex engine. That keeps
//! the binary at its few deps, but a new UI string means a rebuild rather than a config
//! edit. Load the manifests from `~/.config` if agents start changing faster than releases.

use crate::proto::Activity;
use std::ops::Range;
use std::time::{Duration, Instant};
use tui_term::vt100;

/// A launch looks like nothing while the agent paints its splash. herdr waits 3 s too.
pub const STARTUP_GRACE: Duration = Duration::from_secs(3);
/// How long "nothing on screen says working" has to hold before working becomes idle.
/// A turn has gaps between its spinner frames; a prompt box on screen skips the wait.
pub const IDLE_HOLD: Duration = Duration::from_millis(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    /// From the command that launches it, wrappers included: `npx @openai/codex@latest`
    /// and `bash -lc "claude --resume"` both count.
    ///
    /// ponytail: the launch argv, not the PTY's foreground process as herdr reads it, so
    /// an agent started by hand inside a plain shell session is never recognised. Read
    /// `tcgetpgrp` on the master if that turns out to be how people use shells.
    pub fn of(argv: &[String]) -> Option<Agent> {
        argv.iter()
            .flat_map(|a| a.split_whitespace())
            .find_map(|word| {
                let base = word.rsplit('/').next().unwrap_or(word);
                match base.split('@').next().unwrap_or(base) {
                    "claude" | "claude-code" => Some(Agent::Claude),
                    "codex" => Some(Agent::Codex),
                    _ => None,
                }
            })
    }
}

/// One look at the screen. `visible` is idle the screen states outright, a prompt box
/// waiting for input, as opposed to idle by elimination. Only the latter waits out
/// `IDLE_HOLD`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub state: Activity,
    pub visible: bool,
}

const fn says(state: Activity, visible: bool) -> Option<Reading> {
    Some(Reading { state, visible })
}

/// `None` means an overlay that hides the turn behind it, such as the transcript viewer
/// or the model picker. It says nothing, so the last state stands.
pub fn detect(agent: Agent, screen: &str, title: &str) -> Option<Reading> {
    let v = View::new(screen, title);
    let rules = match agent {
        Agent::Claude => CLAUDE,
        Agent::Codex => CODEX,
    };
    match rules.iter().find(|(hit, _)| hit(&v)) {
        Some((_, verdict)) => *verdict,
        // A known agent showing none of its tells is sitting there.
        None => says(Activity::Idle, false),
    }
}

/// The detector's input, as herdr builds it: the visible screen as plain text, each row
/// trimmed, blank rows below the last line of content dropped.
pub fn snapshot(screen: &vt100::Screen) -> String {
    let (_, cols) = screen.size();
    let mut rows: Vec<String> = screen
        .rows(0, cols)
        .map(|r| r.trim_end().to_string())
        .collect();
    while rows.last().is_some_and(|r| r.is_empty()) {
        rows.pop();
    }
    rows.join("\n")
}

/// Turns readings into what clients get to see. Entering working or blocked shows at
/// once. Leaving working for an idle nothing on screen vouches for waits `IDLE_HOLD`,
/// because a turn between two spinner frames looks exactly like that.
pub struct Tracker {
    born: Instant,
    shown: Activity,
    idle_since: Option<Instant>,
}

impl Tracker {
    pub fn new(now: Instant) -> Tracker {
        Tracker {
            born: now,
            shown: Activity::Unknown,
            idle_since: None,
        }
    }

    pub fn shown(&self) -> Activity {
        self.shown
    }

    pub fn starting(&self, now: Instant) -> bool {
        now.duration_since(self.born) < STARTUP_GRACE
    }

    /// Wants another look even if the screen hasn't changed, to confirm a pending idle.
    pub fn pending(&self) -> bool {
        self.idle_since.is_some()
    }

    /// Folds one reading in. True when `shown` changed.
    pub fn step(&mut self, reading: Option<Reading>, now: Instant) -> bool {
        if self.starting(now) {
            return false;
        }
        let Some(r) = reading else {
            return false;
        };
        if self.shown == Activity::Working && r.state == Activity::Idle && !r.visible {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) < IDLE_HOLD {
                return false;
            }
        }
        self.idle_since = None;
        let changed = self.shown != r.state;
        self.shown = r.state;
        changed
    }
}

// --- the screen, cut into herdr's regions -------------------------------------------

struct View<'a> {
    lines: Vec<&'a str>,
    /// `lines` lowercased: herdr's `contains` is case-insensitive.
    lower: Vec<String>,
    title: &'a str,
}

type Lines = Range<usize>;

impl<'a> View<'a> {
    fn new(screen: &'a str, title: &'a str) -> View<'a> {
        let lines: Vec<&str> = screen.lines().map(str::trim_end).collect();
        let lower = lines.iter().map(|l| l.to_lowercase()).collect();
        View {
            lines,
            lower,
            title,
        }
    }

    fn all(&self) -> Lines {
        0..self.lines.len()
    }

    fn is_blank(&self, i: usize) -> bool {
        self.lines[i].trim().is_empty()
    }

    /// From the `n`th non-blank line counting up from the bottom, to the end.
    fn bottom(&self, n: usize) -> Lines {
        let start = (0..self.lines.len())
            .rev()
            .filter(|&i| !self.is_blank(i))
            .take(n)
            .last();
        start.map_or(0..0, |s| s..self.lines.len())
    }

    /// From the top through the `n`th non-blank line.
    fn top(&self, n: usize) -> Lines {
        let end = (0..self.lines.len())
            .filter(|&i| !self.is_blank(i))
            .take(n)
            .last();
        end.map_or(0..0, |e| 0..e + 1)
    }

    fn after_last_rule(&self) -> Lines {
        let start = self
            .lines
            .iter()
            .rposition(|l| is_rule(l))
            .map_or(0, |i| i + 1);
        start..self.lines.len()
    }

    /// Claude's input box is drawn between two rules; this is the upper one.
    fn prompt_box_top(&self) -> Option<usize> {
        (0..self.lines.len())
            .rev()
            .filter(|&i| is_rule(self.lines[i]))
            .nth(1)
    }

    fn prompt_box_body(&self) -> Lines {
        let Some(top) = self.prompt_box_top() else {
            return 0..0;
        };
        let end = (top + 1..self.lines.len())
            .find(|&i| is_rule(self.lines[i]))
            .unwrap_or(self.lines.len());
        top + 1..end
    }

    fn last_line_above_prompt_box(&self) -> Lines {
        let end = self.prompt_box_top().unwrap_or(self.lines.len());
        (0..end)
            .rev()
            .find(|&i| !self.is_blank(i))
            .map_or(0..0, |i| i..i + 1)
    }

    /// Codex's live prompt: the last `›` line, unless a response block started below it.
    fn codex_prompt(&self) -> Option<usize> {
        let i = self.lines.iter().rposition(|l| codex_prompt_line(l))?;
        (!self.lines[i + 1..].iter().any(|l| codex_block_marker(l))).then_some(i)
    }

    fn after_last_codex_prompt(&self) -> Lines {
        let start = self
            .lines
            .iter()
            .rposition(|l| codex_prompt_line(l))
            .map_or(0, |i| i + 1);
        start..self.lines.len()
    }

    fn before_codex_prompt(&self) -> Lines {
        0..self.codex_prompt().unwrap_or(self.lines.len())
    }

    // --- matchers over a region ---

    /// Case-insensitive; `needle` must be lowercase already.
    fn has(&self, r: Lines, needle: &str) -> bool {
        self.lower[r].iter().any(|l| l.contains(needle))
    }

    fn has_all(&self, r: Lines, needles: &[&str]) -> bool {
        needles.iter().all(|n| self.has(r.clone(), n))
    }

    fn has_any(&self, r: Lines, needles: &[&str]) -> bool {
        needles.iter().any(|n| self.has(r.clone(), n))
    }

    fn line(&self, r: Lines, f: impl Fn(&str) -> bool) -> bool {
        self.lines[r].iter().any(|l| f(l))
    }

    /// Lowercased, whitespace and line breaks collapsed to single spaces, for phrases
    /// the agent is free to wrap.
    fn flat(&self, r: Lines) -> String {
        self.lower[r]
            .iter()
            .flat_map(|l| l.split_whitespace())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// `────` across the screen, or a rule of at least three with a label after it.
fn is_rule(line: &str) -> bool {
    let t = line.trim();
    let run = t.chars().take_while(|&c| c == '─').count();
    run > 0 && (run >= 3 || t.trim_start_matches('─').trim_start().is_empty())
}

fn codex_prompt_line(line: &str) -> bool {
    line == "›" || line.starts_with("› ")
}

fn codex_block_marker(line: &str) -> bool {
    line.starts_with(['•', '■', '✗', '✓'])
}

/// Codex animates its idle prompt with a sparkle where the space after `›` would be.
fn codex_sparkle_prompt(line: &str) -> bool {
    line.strip_prefix('›')
        .is_some_and(|r| r.starts_with(['⠁', '⠂', '⠄', '⠈', '⠐', '⠠', '⡀', '⢀']))
}

/// `word` at the start of `s`, case-insensitive, not followed by more of a word.
fn starts_word(s: &str, word: &str) -> bool {
    s.get(..word.len())
        .is_some_and(|p| p.eq_ignore_ascii_case(word))
        && !s[word.len()..].starts_with(|c: char| c.is_alphanumeric() || c == '_')
}

/// Strips the `❯` cursor and whitespace in front of a menu option.
fn option_text(line: &str) -> &str {
    let t = line.trim_start();
    t.strip_prefix('❯').unwrap_or(t).trim_start()
}

/// A numbered yes/no option of a permission prompt: `❯ 1. Yes`, `2. No, and tell…`.
fn yes_no_option(line: &str, bare_yes: bool) -> bool {
    let t = option_text(line);
    let (n, rest) = match t.as_bytes() {
        [d @ b'1'..=b'3', b'.', ..] => (Some(d - b'0'), t[2..].trim_start()),
        _ => (None, t),
    };
    match n {
        None => bare_yes && starts_word(rest, "yes"),
        Some(1) => starts_word(rest, "yes"),
        Some(2) => starts_word(rest, "yes") || starts_word(rest, "no"),
        _ => starts_word(rest, "no"),
    }
}

/// The digits at the front of `s` and what follows them, if there are any.
fn digits(s: &str) -> Option<(&str, &str)> {
    let n = s.bytes().take_while(u8::is_ascii_digit).count();
    (n > 0).then(|| s.split_at(n))
}

// --- Claude Code ---------------------------------------------------------------------

/// Claude's spinner glyphs, the frames of the `✻ Pondering…` line.
const CLAUDE_SPIN: [char; 7] = ['*', '·', '✢', '✳', '✶', '✻', '✽'];

type Rule = (fn(&View) -> bool, Option<Reading>);

const CLAUDE: &[Rule] = &[
    (claude_title_working, says(Activity::Working, true)),
    (claude_transcript_viewer, None),
    (claude_blocked_form, says(Activity::Blocked, true)),
    (claude_workflow_prompt, says(Activity::Blocked, true)),
    (claude_mcp_elicitation, says(Activity::Blocked, true)),
    (claude_btw_overlay, says(Activity::Working, true)),
    (claude_live_turn, says(Activity::Working, true)),
    (claude_background_agents, says(Activity::Working, true)),
    (claude_background_mcp, says(Activity::Working, true)),
    (claude_prompt_box, says(Activity::Idle, true)),
    (claude_model_picker, None),
    (claude_bash_permission, says(Activity::Blocked, true)),
    (claude_generic_permission, says(Activity::Blocked, true)),
    (claude_legacy_blocker, says(Activity::Blocked, false)),
    (claude_title_idle, says(Activity::Idle, true)),
];

/// `osc_title_working`: a braille spinner (≤ 2.1.227) or a half circle (2.1.228+).
fn claude_title_working(v: &View) -> bool {
    let mut c = v.title.chars();
    c.next()
        .is_some_and(|g| matches!(g, '\u{2800}'..='\u{28FF}' | '\u{25D0}'..='\u{25D3}'))
        && c.next() == Some(' ')
}

/// `osc_title_idle`: `✳ Claude Code`.
fn claude_title_idle(v: &View) -> bool {
    v.title.starts_with("✳ ")
}

/// `transcript_viewer` (ctrl+o).
fn claude_transcript_viewer(v: &View) -> bool {
    let r = v.bottom(3);
    v.has(r.clone(), "showing detailed transcript")
        && (v.has_all(r.clone(), &["ctrl+o", "to toggle"])
            || v.has_all(r.clone(), &["ctrl+e", "show all"])
            || v.has_all(r.clone(), &["ctrl+e", "collapse"])
            || v.has_any(r, &["↑↓ scroll", "? for shortcuts"]))
}

const NAVIGATE: [&str; 5] = [
    "tab/arrow keys to navigate",
    "arrow keys to navigate",
    "arrows to navigate",
    "↑/↓ to navigate",
    "↑↓ to navigate",
];

/// `live_blocked_form`: a question or picker under the last rule.
fn claude_blocked_form(v: &View) -> bool {
    let r = v.after_last_rule();
    v.has(r.clone(), "esc to cancel")
        && (v.has(r.clone(), "enter to confirm")
            || (v.has(r.clone(), "enter to select") && v.has_any(r, &NAVIGATE)))
}

/// `dynamic_workflow_prompt`.
fn claude_workflow_prompt(v: &View) -> bool {
    v.has_all(v.all(), &["run a dynamic workflow?", "esc to cancel"])
}

/// `mcp_elicitation_prompt`: `MCP server "x" requests your input` with Accept/Decline.
fn claude_mcp_elicitation(v: &View) -> bool {
    let header = |l: &str| {
        let t = l.trim().to_lowercase();
        (t.starts_with("mcp server \"") || t.starts_with("mcp server “"))
            && (t.ends_with("\" requests your input") || t.ends_with("” requests your input"))
    };
    let control = |l: &str| {
        let t = option_text(l);
        ["Accept", "Decline"].iter().any(|w| {
            t.strip_prefix(w)
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_'))
        })
    };
    v.has(v.all(), "esc to cancel") && v.line(v.all(), header) && v.line(v.all(), control)
}

/// `btw_overlay_working`: a `/btw` side question is being answered.
fn claude_btw_overlay(v: &View) -> bool {
    let r = v.bottom(5);
    v.line(r.clone(), |l| {
        let t = l.trim_start();
        t.strip_prefix("/btw")
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace))
    }) && v.lower[r]
        .iter()
        .any(|l| l.trim_end().ends_with("esc to close"))
}

/// `live_turn_working`: the footer's `esc to interrupt`, or the `✻ Pondering… (12s ·`
/// line.
fn claude_live_turn(v: &View) -> bool {
    v.line(v.bottom(12), |l| {
        let t = l.trim_start();
        if t.starts_with(['⏸', '⏵']) {
            return t.match_indices("esc to interrupt").any(|(i, m)| {
                let rest = &t[i + m.len()..];
                rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with('·')
            });
        }
        let Some(rest) = t.strip_prefix(CLAUDE_SPIN) else {
            return false;
        };
        let body = rest.trim_start();
        if body.len() == rest.len() || body.is_empty() {
            return false;
        }
        body.match_indices('…').any(|(i, m)| {
            let after = &body[i + m.len()..];
            i > 0 && (after.trim().is_empty() || elapsed_follows(after))
        })
    })
}

/// ` (12s ·`, or ` (3m ` — the turn's timer right after the spinner verb.
fn elapsed_follows(s: &str) -> bool {
    let t = s.trim_start();
    if t.len() == s.len() {
        return false;
    }
    let Some((_, rest)) = t.strip_prefix('(').and_then(digits) else {
        return false;
    };
    let mut c = rest.chars();
    matches!(c.next(), Some('s' | 'm' | 'h'))
        && c.next().is_some_and(|ch| ch.is_whitespace() || ch == '·')
}

/// `background_agents_working`: `✶ Waiting for 2 background agents to finish`.
fn claude_background_agents(v: &View) -> bool {
    v.line(v.last_line_above_prompt_box(), |l| {
        let t = l.trim();
        let Some(rest) = t.strip_prefix(['*', '·', '✢', '✶', '✻', '✽']) else {
            return false;
        };
        let body = rest.trim_start();
        if body.len() == rest.len() {
            return false;
        }
        let Some((n, tail)) = body.strip_prefix("Waiting for ").and_then(digits) else {
            return false;
        };
        !n.starts_with('0')
            && matches!(
                tail,
                " background agent to finish" | " background agents to finish"
            )
    })
}

const CLAUDE_ASKING: [&str; 6] = [
    "do you want to proceed?",
    "esc to cancel",
    "waiting for permission",
    "do you want to allow this connection?",
    "tab to amend",
    "ctrl+e to explain",
];

/// `background_mcp_task_working`: an activity line at column zero whose summary, maybe
/// wrapped over indented lines, ends `· 2 MCP tasks still running`.
fn claude_background_mcp(v: &View) -> bool {
    let r = v.bottom(12);
    if v.has_any(r.clone(), &CLAUDE_ASKING) {
        return false;
    }
    r.clone().any(|i| {
        let l = v.lines[i];
        let Some(rest) = l.strip_prefix(['*', '·', '✢', '✶', '✻', '✽']) else {
            return false;
        };
        if !rest.starts_with([' ', '\t']) || rest.trim().is_empty() {
            return false;
        }
        let wrapped = (i + 1..r.end)
            .take(3)
            .take_while(|&j| v.lines[j].starts_with([' ', '\t']))
            .map(|j| v.lines[j]);
        let text: Vec<&str> = std::iter::once(rest)
            .chain(wrapped)
            .flat_map(str::split_whitespace)
            .collect();
        mcp_tasks_tail(&text)
    })
}

/// `… · 2 MCP tasks still running` at the end of a whitespace-split line.
fn mcp_tasks_tail(words: &[&str]) -> bool {
    let n = words.len();
    if n < 5 || words[n - 2..] != ["still", "running"] || !matches!(words[n - 3], "task" | "tasks")
    {
        return false;
    }
    let count = words[n - 5];
    let count = count.strip_prefix('·').unwrap_or(count);
    let dot_before = words[n - 5].starts_with('·') || (n >= 6 && words[n - 6].ends_with('·'));
    words[n - 4] == "MCP"
        && dot_before
        && !count.is_empty()
        && !count.starts_with('0')
        && count.bytes().all(|b| b.is_ascii_digit())
}

/// `live_prompt_box`: a `❯` in the input box, with no picker taking it over.
fn claude_prompt_box(v: &View) -> bool {
    let r = v.prompt_box_body();
    v.line(r.clone(), |l| l.trim_start().starts_with('❯'))
        && !v.has_any(
            r,
            &[
                "enter to select",
                "esc to cancel",
                "tab/arrow keys",
                "arrow keys to navigate",
                "↑/↓ to navigate",
            ],
        )
}

/// `model_picker_menu`.
fn claude_model_picker(v: &View) -> bool {
    let r = v.all();
    v.has_all(
        r.clone(),
        &["select model", "enter to set as default", "esc to cancel"],
    ) && !v.has_any(r, &["do you want to proceed?", "enter to select"])
}

/// `bash_permission_prompt`.
fn claude_bash_permission(v: &View) -> bool {
    let r = v.all();
    v.has(r.clone(), "do you want to proceed?")
        && v.has_any(
            r.clone(),
            &[
                "bash command",
                "bash(",
                "contains expansion",
                "tab to amend",
                "ctrl+e to explain",
            ],
        )
        && v.line(r, |l| yes_no_option(l, true))
}

/// `generic_permission_prompt`: an edit, a fetch, an MCP tool.
fn claude_generic_permission(v: &View) -> bool {
    let r = v.after_last_rule();
    v.has_all(r.clone(), &["do you want to proceed?", "esc to cancel"])
        && v.line(r, |l| yes_no_option(l, false))
}

/// `legacy_no_prompt_blocker`: older dialogs, unless an empty prompt says otherwise.
fn claude_legacy_blocker(v: &View) -> bool {
    let r = v.all();
    let yes_or_cursor = |q: &str| v.has(r.clone(), q) && v.has_any(r.clone(), &["yes", "❯"]);
    let asks = yes_or_cursor("do you want to")
        || yes_or_cursor("would you like to")
        || v.has_any(
            r.clone(),
            &[
                "waiting for permission",
                "do you want to allow this connection?",
                "tab to amend",
                "ctrl+e to explain",
                "review your answers",
                "skip interview and plan immediately",
            ],
        )
        || v.has_all(r.clone(), &["do you want to proceed?", "esc to cancel"]);
    asks && !v.line(r, |l| l.trim() == "❯")
}

// --- Codex ---------------------------------------------------------------------------

const CODEX: &[Rule] = &[
    (codex_title_blocked, says(Activity::Blocked, true)),
    (codex_title_working, says(Activity::Working, true)),
    (codex_transcript_viewer, None),
    (codex_trust_directory, says(Activity::Blocked, true)),
    (codex_startup_update, says(Activity::Blocked, true)),
    (codex_strong_blocker, says(Activity::Blocked, true)),
    (codex_weak_blocker, says(Activity::Blocked, false)),
    (codex_working, says(Activity::Working, true)),
    (codex_title_idle, says(Activity::Idle, true)),
];

const CODEX_SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// `osc_title_blocked`.
fn codex_title_blocked(v: &View) -> bool {
    v.title.to_lowercase().contains("action required")
}

/// `osc_title_working`: a braille frame as a word of its own.
fn codex_title_working(v: &View) -> bool {
    v.title.split(' ').any(|w| {
        let mut c = w.chars();
        c.next().is_some_and(|g| CODEX_SPIN.contains(&g)) && c.next().is_none()
    })
}

/// `osc_title_idle`: any other title.
fn codex_title_idle(v: &View) -> bool {
    !v.title.trim().is_empty()
}

/// `transcript_viewer`.
fn codex_transcript_viewer(v: &View) -> bool {
    let r = v.after_last_codex_prompt();
    v.has_all(
        r.clone(),
        &[
            "↑/↓ to scroll",
            "pgup/pgdn to",
            "home/end to jump",
            "q to quit",
        ],
    ) && v.has_any(r, &["esc to edit prev", "esc/← to edit prev"])
}

/// `trust_directory`: the first-run "Do you trust the contents of this directory?".
fn codex_trust_directory(v: &View) -> bool {
    let r = v.top(20);
    let header = v.lines.first().is_some_and(|l| {
        l.strip_prefix("> You are in ")
            .is_some_and(|rest| !rest.is_empty())
    }) || v.has(r.clone(), "folder access");
    let question = v
        .flat(r.clone())
        .contains("do you trust the contents of this directory?")
        || (v.has_all(
            r.clone(),
            &[
                "trust this folder?",
                "codex can read, edit, and run files here",
            ],
        ) && v.has_any(r, &["trust and continue", "enter continue"]));
    header && question
}

/// `startup_update`.
fn codex_startup_update(v: &View) -> bool {
    let r = v.bottom(20);
    v.has_all(r.clone(), &["update available!", "update now"])
        && v.flat(r.clone()).contains("skip until next version")
        && v.lines[r]
            .last()
            .is_some_and(|l| l.ends_with("Press enter to continue"))
}

/// `live_strong_blocker`: an approval or question below the prompt.
fn codex_strong_blocker(v: &View) -> bool {
    let r = v.after_last_codex_prompt();
    v.has_any(
        r.clone(),
        &[
            "press enter to confirm or esc to cancel",
            "enter to submit answer",
            "enter to submit all",
            "allow command?",
        ],
    ) || v.has_all(r, &["all results", "filesystem only", "plugins"])
}

/// `weak_blocker`: a y/n question, unless the prompt is live.
fn codex_weak_blocker(v: &View) -> bool {
    if v.codex_prompt().is_some() {
        return false;
    }
    let r = v.all();
    let live_sparkle = (0..v.lines.len()).any(|i| {
        codex_sparkle_prompt(v.lines[i]) && !v.lines[i + 1..].iter().any(|l| codex_block_marker(l))
    });
    let yes_or_cursor = |q: &str| v.has(r.clone(), q) && v.has_any(r.clone(), &["yes", "❯"]);
    !live_sparkle
        && (v.has_any(r.clone(), &["[y/n]", "yes (y)"])
            || yes_or_cursor("do you want to")
            || yes_or_cursor("would you like to"))
}

/// `screen_working_fallback`: `• Working (12s • esc to interrupt)` with nothing after it
/// but queued messages.
fn codex_working(v: &View) -> bool {
    let r = v.before_codex_prompt();
    let lines = &v.lines[r];
    if lines
        .iter()
        .any(|l| l.contains("Reconnect failed — check the endpoint, then relaunch ("))
    {
        return false;
    }
    (0..lines.len())
        .any(|i| codex_timer_line(lines[i]) && lines[i + 1..].iter().all(|l| codex_trailer(l)))
}

/// A status line ending in its elapsed timer: `Working (1m 05s • esc to interrupt)`.
fn codex_timer_line(line: &str) -> bool {
    let t = match line.strip_prefix(['•', '◦']) {
        Some(rest) if rest.starts_with([' ', '\t']) => rest.trim_start_matches([' ', '\t']),
        Some(_) => return false,
        None => line,
    };
    if t.starts_with(|c: char| c.is_whitespace() || "›•◦■✗✓─".contains(c)) || t.is_empty()
    {
        return false;
    }
    t.match_indices(" (").any(|(i, m)| {
        let Some(rest) = elapsed(&t[i + m.len()..]) else {
            return false;
        };
        if let Some(tail) = rest.strip_prefix(')') {
            return tail.is_empty() || tail.starts_with(" · ");
        }
        let Some(hint) = rest.strip_prefix(" • ") else {
            return false;
        };
        hint.match_indices(" to interrupt)").any(|(j, m)| {
            let tail = &hint[j + m.len()..];
            j > 0 && (tail.is_empty() || tail.starts_with(" · "))
        })
    })
}

/// `1h 2m 30s`, returning what follows the seconds.
fn elapsed(s: &str) -> Option<&str> {
    let mut s = s;
    loop {
        let (_, rest) = digits(s)?;
        if let Some(rest) = rest.strip_prefix('s') {
            return Some(rest);
        }
        s = rest
            .strip_prefix("h ")
            .or_else(|| rest.strip_prefix("m "))?;
    }
}

/// What may sit under a live Codex status line: blank space, queued follow-ups, the
/// sparkle prompt, or anything that isn't the start of a new block.
fn codex_trailer(line: &str) -> bool {
    if line.is_empty() || codex_sparkle_prompt(line) {
        return true;
    }
    if let Some(rest) = line.strip_prefix('•') {
        let words: Vec<&str> = rest.split_whitespace().collect();
        let text = words.join(" ");
        return rest.starts_with([' ', '\t'])
            && (text.starts_with("Queued follow-up inputs")
                || text.starts_with("Messages to be submitted after next tool call")
                || text.starts_with("Messages to be submitted at end of turn"));
    }
    !line.starts_with(|c: char| "◦›■✗✓─".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Activity::*;

    fn claude(screen: &str, title: &str) -> Option<Activity> {
        detect(Agent::Claude, screen, title).map(|r| r.state)
    }

    fn codex(screen: &str, title: &str) -> Option<Activity> {
        detect(Agent::Codex, screen, title).map(|r| r.state)
    }

    const RULE: &str = "────────────────────────────────────────────────────────";

    fn prompt_box(above: &str, footer: &str) -> String {
        format!("{above}\n\n{RULE}\n❯ \n{RULE}\n  {footer}\n")
    }

    #[test]
    fn agents_are_recognised_through_their_wrappers() {
        let argv = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(
            Agent::of(&argv(&["claude", "--resume"])),
            Some(Agent::Claude)
        );
        assert_eq!(
            Agent::of(&argv(&["/opt/homebrew/bin/claude"])),
            Some(Agent::Claude)
        );
        assert_eq!(
            Agent::of(&argv(&["npx", "@openai/codex@latest"])),
            Some(Agent::Codex)
        );
        assert_eq!(
            Agent::of(&argv(&["bash", "-lc", "cd ~/x && claude --continue"])),
            Some(Agent::Claude)
        );
        assert_eq!(Agent::of(&argv(&["/bin/zsh", "-l"])), None);
        // A model name is not the agent.
        assert_eq!(Agent::of(&argv(&["aider", "--model", "claude-3"])), None);
    }

    #[test]
    fn claude_idle_at_its_prompt() {
        let s = prompt_box("● Done. The tests pass.", "? for shortcuts");
        assert_eq!(detect(Agent::Claude, &s, ""), says(Idle, true));
    }

    #[test]
    fn claude_working_under_a_live_turn() {
        // The prompt box is there while a turn runs too; the spinner line outranks it.
        let s = prompt_box(
            "● Reading the file.\n\n✻ Pondering… (12s · ↓ 340 tokens · esc to interrupt)",
            "⏵⏵ accept edits on (shift+tab to cycle)",
        );
        assert_eq!(claude(&s, ""), Some(Working));

        let footer = prompt_box("● Reading.", "⏵⏵ accept edits on · esc to interrupt");
        assert_eq!(claude(&footer, ""), Some(Working));

        let bare = prompt_box("✶ Cogitating…", "? for shortcuts");
        assert_eq!(claude(&bare, ""), Some(Working));
    }

    #[test]
    fn a_finished_turn_summary_is_not_a_live_one() {
        // What Claude leaves behind once the turn is over: no ellipsis, no live timer.
        let s = prompt_box("✻ Worked for 1m 12s", "? for shortcuts");
        assert_eq!(claude(&s, ""), Some(Idle));
    }

    #[test]
    fn claude_title_outranks_the_screen() {
        let s = prompt_box("● Done.", "? for shortcuts");
        assert_eq!(claude(&s, "⠂ Fix the login bug"), Some(Working));
        assert_eq!(claude(&s, "◐ Fix the login bug"), Some(Working));
        assert_eq!(claude("", "✳ Claude Code"), Some(Idle));
    }

    #[test]
    fn claude_blocked_on_a_bash_permission() {
        let s = format!(
            "● Bash(rm -rf build)\n{RULE}\n Bash command\n\n   rm -rf build\n   Remove the build dir\n\n \
             Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for rm commands\n   \
             3. No, and tell Claude what to do differently (esc)\n\n Esc to cancel · Tab to amend"
        );
        assert_eq!(
            detect(Agent::Claude, &s, "✳ Claude Code"),
            says(Blocked, true)
        );
    }

    #[test]
    fn claude_blocked_on_an_edit_permission() {
        let s = format!(
            "{RULE}\n Edit file\n src/main.rs\n\n Do you want to proceed?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel"
        );
        assert_eq!(claude(&s, ""), Some(Blocked));
    }

    #[test]
    fn claude_blocked_on_a_question() {
        let s = format!(
            "{RULE}\n Which database?\n ❯ 1. Postgres\n   2. SQLite\n\n Enter to select · ↑/↓ to navigate · Esc to cancel"
        );
        assert_eq!(claude(&s, ""), Some(Blocked));
    }

    #[test]
    fn claude_overlays_keep_the_last_state() {
        let s = "● stuff\n  Showing detailed transcript · ctrl+o to toggle";
        assert_eq!(claude(s, ""), None);
    }

    #[test]
    fn claude_waiting_on_background_work() {
        let agents = prompt_box(
            "✶ Waiting for 2 background agents to finish",
            "? for shortcuts",
        );
        assert_eq!(claude(&agents, ""), Some(Working));
        let mcp = prompt_box(
            "✶ Querying the index · 1 MCP task\n  still running",
            "? for shortcuts",
        );
        assert_eq!(claude(&mcp, ""), Some(Working));
    }

    #[test]
    fn claude_shows_nothing_known() {
        assert_eq!(
            detect(Agent::Claude, "Welcome to Claude Code", ""),
            says(Idle, false)
        );
    }

    #[test]
    fn codex_working_and_idle() {
        let working = "› fix it\n\n• Working (12s • esc to interrupt)\n\n› \n";
        // The `›` under the status is the live prompt, so the status is above it.
        assert_eq!(codex(working, ""), Some(Working));
        let longer = "• Running tests (1m 05s • esc to interrupt) · 3 queued\n";
        assert_eq!(codex(longer, ""), Some(Working));
        let done = "• Working (12s • esc to interrupt)\n• Done. Tests pass.\n\n› \n";
        assert_eq!(codex(done, ""), Some(Idle));
        assert_eq!(codex("", "⠙ codex"), Some(Working));
        assert_eq!(codex("", "codex"), Some(Idle));
    }

    #[test]
    fn codex_blocked() {
        assert_eq!(codex("", "Action Required · codex"), Some(Blocked));
        let approval = "› run the tests\n\n  Allow command?\n  $ cargo test\n  Press enter to confirm or esc to cancel";
        assert_eq!(codex(approval, ""), Some(Blocked));
        let trust =
            "> You are in /home/me/x\n\n  Do you trust the contents of\n  this directory?\n";
        assert_eq!(codex(trust, ""), Some(Blocked));
    }

    #[test]
    fn rules_and_options_parse_like_herdr() {
        assert!(is_rule("────"));
        assert!(is_rule("─── label"));
        assert!(is_rule("─"));
        assert!(!is_rule("─ x"));
        assert!(!is_rule("text ───"));
        assert!(yes_no_option(" ❯ 1. Yes", false));
        assert!(yes_no_option("   3. No, and tell Claude", false));
        assert!(!yes_no_option("   3. Yesterday", false));
        assert!(!yes_no_option(" Yes", false));
        assert!(yes_no_option(" Yes", true));
    }

    #[test]
    fn working_holds_through_a_gap_but_not_past_a_prompt() {
        let t0 = Instant::now();
        let mut t = Tracker::new(t0);
        let at = |ms: u64| t0 + STARTUP_GRACE + Duration::from_millis(ms);
        assert!(!t.step(says(Working, true), t0), "the splash says nothing");
        assert_eq!(t.shown(), Unknown);

        assert!(t.step(says(Working, true), at(0)));
        assert!(!t.step(says(Idle, false), at(100)), "a gap between frames");
        assert!(t.pending());
        assert!(!t.step(says(Working, true), at(200)));
        assert!(!t.pending(), "the turn resumed, so the hold starts over");
        assert!(!t.step(says(Idle, false), at(300)));
        assert!(t.step(says(Idle, false), at(300) + IDLE_HOLD));
        assert_eq!(t.shown(), Idle);

        assert!(t.step(says(Working, true), at(1000)));
        assert!(
            t.step(says(Idle, true), at(1001)),
            "a prompt box needs no hold"
        );
        assert!(t.step(says(Blocked, true), at(1002)));
        assert!(!t.step(None, at(1003)), "an overlay changes nothing");
        assert_eq!(t.shown(), Blocked);
    }

    #[test]
    fn snapshot_drops_the_blank_bottom() {
        let mut p = vt100::Parser::new(6, 20, 0);
        p.process(b"one   \r\ntwo\r\n");
        assert_eq!(snapshot(p.screen()), "one\ntwo");
    }
}
