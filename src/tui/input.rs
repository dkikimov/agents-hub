//! Translation between what the host terminal reports and what the guest expects:
//! key and mouse bytes, paste framing, and the hit-testing that turns a click's
//! screen coordinates into a pane cell or a sidebar row. All pure functions.

use super::app::Row;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tui_term::vt100;
use tui_term::vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// crossterm key → the bytes a real terminal would have sent.
pub fn key_bytes(k: KeyEvent) -> Option<Vec<u8>> {
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
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = text
        .replace("\r\n", "\r")
        .replace('\n', "\r")
        .replace("\x1b[201~", "");
    if bracketed {
        format!("\x1b[200~{body}\x1b[201~").into_bytes()
    } else {
        body.into_bytes()
    }
}

pub fn scroll_screen(screen: &mut vt100::Screen, up: bool, rows: usize) {
    let at = screen.scrollback();
    screen.set_scrollback(if up {
        at.saturating_add(rows)
    } else {
        at.saturating_sub(rows)
    });
}

/// Screen coordinates → a cell inside the pane. `clamp` is for a drag in progress,
/// which should follow the pointer out of the pane instead of stopping dead.
pub fn pane_cell(
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

pub fn selected_text(screen: &vt100::Screen, start: (u16, u16), end: (u16, u16)) -> Option<String> {
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
    let text = screen.contents_between(start.1, start.0, end.1, end.0.saturating_add(1).min(cols));
    (!text.is_empty()).then_some(text)
}

const TRIM: [char; 12] = ['(', ')', '[', ']', '{', '}', '<', '>', '\'', '"', '`', ','];

/// The URL under a cell, joined across the pane's own wrapping so a link that ran off the
/// right edge still opens whole. The scheme is found inside the clicked word rather than at
/// its start, so `[docs](https://x)` and `"https://x"` both resolve.
pub fn url_at(screen: &vt100::Screen, (col, row): (u16, u16)) -> Option<String> {
    let (rows, cols) = screen.size();
    if col >= cols || row >= rows {
        return None;
    }
    let (mut first, mut last) = (row, row);
    while first > 0 && screen.row_wrapped(first - 1) {
        first -= 1;
    }
    while last + 1 < rows && screen.row_wrapped(last) {
        last += 1;
    }
    let cells: Vec<&str> = (first..=last)
        .flat_map(|r| (0..cols).map(move |c| (r, c)))
        .map(|(r, c)| screen.cell(r, c).map_or("", vt100::Cell::contents))
        .collect();

    let blank = |s: &&str| s.is_empty() || s.chars().all(char::is_whitespace);
    let at = usize::from(row - first) * usize::from(cols) + usize::from(col);
    if blank(&cells[at]) {
        return None;
    }
    let start = cells[..at].iter().rposition(blank).map_or(0, |i| i + 1);
    let end = cells[at..]
        .iter()
        .position(blank)
        .map_or(cells.len(), |i| at + i);
    let word = cells[start..end].concat();

    let scheme_end = word.find("://")?;
    let scheme_start = word[..scheme_end]
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || "+-.".contains(c)))
        .map_or(0, |i| i + 1);
    let scheme = &word[scheme_start..scheme_end];
    if !scheme.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return None;
    }
    let url = word[scheme_start..].trim_end_matches(TRIM);
    (url.len() > scheme_end - scheme_start + 3).then(|| url.to_string())
}

/// crossterm mouse event → the bytes a real terminal would send, in whatever protocol
/// the app inside the pane turned on. `None` means that app didn't ask for this event
/// (or for any mouse at all), and we should handle it ourselves.
pub fn mouse_bytes(m: MouseEvent, screen: &vt100::Screen, col: u16, row: u16) -> Option<Vec<u8>> {
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
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
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

/// A press and release that never moved: still a click as far as the guest is concerned.
pub fn click_bytes(
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
pub fn on_split(col: u16, side_w: u16) -> bool {
    col + 1 >= side_w && col <= side_w
}

/// Screen row of a sidebar click → the row it landed on. `top` is the list's first line
/// (under the border) and `off` how far it's scrolled; the inert "…" rows aren't clickable.
pub fn row_at(rows: &[Row], off: usize, top: u16, y: u16) -> Option<usize> {
    let i = off + usize::from(y.checked_sub(top)?);
    match rows.get(i)? {
        Row::Elide(_) => None,
        _ => Some(i),
    }
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

    #[test]
    fn paste_is_one_message_not_one_per_line() {
        let out = paste_bytes("a\r\nb\nc\x1b[201~d", true);
        assert_eq!(out, b"\x1b[200~a\rb\rcd\x1b[201~".to_vec());
        assert_eq!(paste_bytes("a\nb", false), b"a\rb".to_vec());
    }

    #[test]
    fn drag_selection_is_inclusive_and_direction_independent() {
        let mut parser = vt100::Parser::new(3, 10, 0);
        parser.process(b"alpha\r\nbeta");

        assert_eq!(
            selected_text(parser.screen(), (1, 0), (1, 1)),
            Some("lpha\nbe".into())
        );
        assert_eq!(
            selected_text(parser.screen(), (1, 1), (1, 0)),
            Some("lpha\nbe".into())
        );
        assert_eq!(selected_text(parser.screen(), (1, 0), (1, 0)), None);
    }

    #[test]
    fn stationary_left_drag_remains_a_child_click() {
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
    fn a_url_is_found_under_the_click_even_when_the_pane_wrapped_it() {
        let mut parser = vt100::Parser::new(4, 20, 0);
        parser.process(b"see https://example.com/a/long/path now\r\nnope");
        let url = "https://example.com/a/long/path";

        assert_eq!(url_at(parser.screen(), (6, 0)).as_deref(), Some(url));
        assert_eq!(url_at(parser.screen(), (2, 1)).as_deref(), Some(url));
        assert_eq!(url_at(parser.screen(), (1, 0)), None); // "see"
        assert_eq!(url_at(parser.screen(), (17, 1)), None); // "now"
        assert_eq!(url_at(parser.screen(), (1, 2)), None); // "nope", past the wrap
        assert_eq!(url_at(parser.screen(), (0, 3)), None); // blank
        assert_eq!(url_at(parser.screen(), (40, 0)), None); // off-screen

        let mut parser = vt100::Parser::new(2, 40, 0);
        parser.process(b"[docs](https://x.dev/p), ftp://h/f, a://b");
        assert_eq!(
            url_at(parser.screen(), (10, 0)).as_deref(),
            Some("https://x.dev/p")
        );
        assert_eq!(
            url_at(parser.screen(), (26, 0)).as_deref(),
            Some("ftp://h/f")
        );
        assert_eq!(url_at(parser.screen(), (37, 0)).as_deref(), Some("a://b"));
    }

    #[test]
    fn mouse_bytes_match_a_real_terminal() {
        let ev = |kind| MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
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
        assert_eq!(
            b(MouseEventKind::Down(MouseButton::Left), 2, 3).unwrap(),
            "\x1b[<0;3;4M"
        );
        assert_eq!(
            b(MouseEventKind::Up(MouseButton::Left), 2, 3).unwrap(),
            "\x1b[<0;3;4m"
        );
        assert_eq!(
            b(MouseEventKind::Drag(MouseButton::Left), 2, 3).unwrap(),
            "\x1b[<32;3;4M"
        );
        assert_eq!(b(MouseEventKind::Moved, 2, 3).unwrap(), "\x1b[<35;3;4M");

        // Press/release only (?1000h): drags and moves stay with us.
        let vt200 = screen("\x1b[?1000h\x1b[?1006h");
        assert!(mouse_bytes(ev(MouseEventKind::Moved), vt200.screen(), 1, 1).is_none());
        assert!(
            mouse_bytes(ev(MouseEventKind::Drag(MouseButton::Left)), vt200.screen(), 1, 1).is_none()
        );
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
}
