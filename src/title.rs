//! Window titles out of a PTY byte stream: the `ESC ] 0 ; text BEL` an agent sets when it
//! has decided what a session is about. Claude Code re-sends it every turn; the daemon reads
//! it so the session's name follows without any client attached.

/// Longest title we keep. Anything longer is a screenful of something else, not a name.
const MAX_OSC: usize = 256;
const MAX_TITLE: usize = 60;

#[derive(Default)]
enum State {
    #[default]
    Ground,
    Esc,
    /// Inside an OSC. `buf` stops growing at `MAX_OSC`, and a sequence that overflows it is
    /// dropped whole rather than truncated into a plausible-looking title.
    Osc { buf: Vec<u8>, overflow: bool },
    /// An ESC seen inside an OSC: `\` ends it, anything else abandons it.
    OscEsc { buf: Vec<u8>, overflow: bool },
}

/// Feeds on chunks as `read()` hands them over, so a sequence split across two chunks
/// (8 KB reads land anywhere) still completes.
#[derive(Default)]
pub struct TitleScanner(State);

impl TitleScanner {
    /// The last complete title set within `chunk`, if any.
    pub fn feed(&mut self, chunk: &[u8]) -> Option<String> {
        let mut found = None;
        for &b in chunk {
            self.0 = match std::mem::take(&mut self.0) {
                State::Ground | State::Esc if b == 0x1b => State::Esc,
                State::Esc if b == b']' => State::Osc { buf: Vec::new(), overflow: false },
                State::Ground | State::Esc => State::Ground,
                State::Osc { buf, overflow } if b == 0x1b => State::OscEsc { buf, overflow },
                State::Osc { buf, overflow } if b == 0x07 => {
                    found = title_of(&buf, overflow).or(found);
                    State::Ground
                }
                // CAN and SUB cancel a control string.
                State::Osc { .. } if b == 0x18 || b == 0x1a => State::Ground,
                State::Osc { mut buf, overflow } => {
                    if buf.len() < MAX_OSC {
                        buf.push(b);
                        State::Osc { buf, overflow }
                    } else {
                        State::Osc { buf, overflow: true }
                    }
                }
                State::OscEsc { buf, overflow } if b == b'\\' => {
                    found = title_of(&buf, overflow).or(found);
                    State::Ground
                }
                State::OscEsc { .. } if b == b']' => State::Osc { buf: Vec::new(), overflow: false },
                State::OscEsc { .. } => State::Ground,
            };
        }
        found
    }
}

/// OSC 0 (icon + title) and OSC 2 (title). OSC 1 is the icon name alone, not a title.
fn title_of(buf: &[u8], overflow: bool) -> Option<String> {
    if overflow {
        return None;
    }
    let rest = buf.strip_prefix(b"0;").or_else(|| buf.strip_prefix(b"2;"))?;
    Some(String::from_utf8_lossy(rest).into_owned())
}

/// Turns a raw title into a session name, or `None` when it says nothing about the work.
///
/// Claude Code leads its title with a status glyph that animates while it thinks
/// (`✳`, or a braille spinner), so the same title arrives as many different strings; the
/// glyph is dropped so it doesn't rename the session ten times a second. A title that is
/// just the agent's own name is what it shows before it has anything to say.
pub fn clean(raw: &str, agent: &str) -> Option<String> {
    let text: String = raw.chars().filter(|c| !c.is_control()).collect();
    let text = text.trim_start_matches(is_status_glyph).trim();
    if text.is_empty() {
        return None;
    }
    let generic = ["claude", "claude code", "codex", "openai codex", agent];
    if generic.iter().any(|g| text.to_lowercase() == g.to_lowercase()) {
        return None;
    }
    Some(match text.char_indices().nth(MAX_TITLE) {
        Some((i, _)) => format!("{}…", text[..i].trim_end()),
        None => text.to_string(),
    })
}

fn is_status_glyph(c: char) -> bool {
    c.is_whitespace()
        || matches!(c, '*' | '·' | '•' | '●' | '○' | '◐'..='◓')
        || ('\u{2700}'..='\u{27bf}').contains(&c) // dingbats: ✳ ✻ ✽
        || ('\u{2800}'..='\u{28ff}').contains(&c) // braille spinner frames
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(chunks: &[&[u8]]) -> Vec<String> {
        let mut s = TitleScanner::default();
        chunks.iter().filter_map(|c| s.feed(c)).collect()
    }

    #[test]
    fn reads_a_title_ended_by_bel_or_st() {
        assert_eq!(scan(&[b"\x1b]0;fix login\x07"]), ["fix login"]);
        assert_eq!(scan(&[b"\x1b]2;fix login\x1b\\"]), ["fix login"]);
    }

    #[test]
    fn a_title_split_across_reads_still_completes() {
        assert_eq!(scan(&[b"junk\x1b]0;fix lo", b"gin\x07more"]), ["fix login"]);
        assert_eq!(scan(&[b"\x1b", b"]0;a", b"b\x1b", b"\\"]), ["ab"]);
    }

    #[test]
    fn the_last_title_in_a_chunk_wins() {
        assert_eq!(scan(&[b"\x1b]0;one\x07\x1b]0;two\x07"]), ["two"]);
    }

    #[test]
    fn other_osc_sequences_are_not_titles() {
        // clipboard, cwd, hyperlinks, and OSC 1 (icon name only)
        assert!(scan(&[b"\x1b]52;c;aGVsbG8=\x07\x1b]7;file:///tmp\x07\x1b]1;x\x07"]).is_empty());
        assert!(scan(&[b"\x1b]8;;https://x.dev\x1b\\link\x1b]8;;\x1b\\"]).is_empty());
    }

    #[test]
    fn an_oversized_or_cancelled_sequence_is_dropped() {
        let mut big = b"\x1b]0;".to_vec();
        big.extend(std::iter::repeat_n(b'a', MAX_OSC + 50));
        big.push(0x07);
        assert!(scan(&[&big]).is_empty());
        assert!(scan(&[b"\x1b]0;half\x18 tail\x07"]).is_empty());
        // and the scanner is usable again afterwards
        assert_eq!(scan(&[&big, b"\x1b]0;ok\x07"]), ["ok"]);
    }

    #[test]
    fn plain_output_and_csi_do_not_confuse_it() {
        assert!(scan(&[b"hello \x1b[31mred\x1b[0m\r\n]0;not a title\x07"]).is_empty());
    }

    #[test]
    fn the_status_glyph_is_not_part_of_the_name() {
        assert_eq!(clean("✳ Fix login bug", "claude").as_deref(), Some("Fix login bug"));
        assert_eq!(clean("⠂ Fix login bug", "claude").as_deref(), Some("Fix login bug"));
        assert_eq!(clean("⠐ Fix login bug", "claude"), clean("✻ Fix login bug", "claude"));
        assert_eq!(clean("  spaced  ", "claude").as_deref(), Some("spaced"));
    }

    #[test]
    fn a_title_that_says_nothing_is_not_a_name() {
        for t in ["", "✳", "⠋ ", "✳ Claude Code", "Claude Code", "codex", "CLAUDE", "\x1b"] {
            assert_eq!(clean(t, "claude"), None, "{t:?}");
        }
        assert_eq!(clean("mybot", "mybot"), None, "the agent's own configured name");
    }

    #[test]
    fn long_titles_are_cut_on_a_char_boundary() {
        let long = "é".repeat(200);
        let name = clean(&long, "claude").unwrap();
        assert_eq!(name.chars().count(), MAX_TITLE + 1);
        assert!(name.ends_with('…'));
    }
}
