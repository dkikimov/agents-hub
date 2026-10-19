//! Which Claude Code conversation a session is in. `claude --continue` resumes the
//! newest conversation in the cwd, so two sessions in one folder resumed each other's;
//! each session instead carries its own conversation id, which `{conversation}` in an
//! agent's argv stands for.

use std::io::Read;
use std::path::{Path, PathBuf};

pub const PLACEHOLDER: &str = "{conversation}";

/// What a Claude Code process exports to its children. A daemon started from inside
/// one (`make run` in a Claude session) would hand them on, and every agent it launched
/// would then write into that conversation and not its own.
pub const PARENT_ENV: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_ENTRYPOINT",
];

/// A random v4 UUID, the only shape `claude --session-id` accepts.
pub fn new_id() -> std::io::Result<String> {
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

pub fn fill(argv: &[String], id: &str) -> Vec<String> {
    argv.iter().map(|a| a.replace(PLACEHOLDER, id)).collect()
}

pub fn names_one(argv: &[String]) -> bool {
    argv.iter().any(|a| a.contains(PLACEHOLDER))
}

fn claude_dir() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::home().join(".claude"))
}

/// The conversation the Claude process `pid` is in now, which moves on `/clear` and
/// `/resume`. Claude Code keeps it in `sessions/<pid>.json` and deletes that on exit,
/// so this only answers while the process runs.
pub fn current(pid: u32) -> Option<String> {
    current_in(&claude_dir(), pid)
}

fn current_in(dir: &Path, pid: u32) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("sessions").join(format!("{pid}.json"))).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let id = v.get("sessionId")?.as_str()?;
    let well_formed = !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    well_formed.then(|| id.to_string())
}

/// Claude writes nothing until the first message, and `--resume` of an id with no
/// transcript exits with an error rather than starting fresh.
pub fn has_transcript(id: &str) -> bool {
    has_transcript_in(&claude_dir(), id)
}

fn has_transcript_in(dir: &Path, id: &str) -> bool {
    let Ok(projects) = std::fs::read_dir(dir.join("projects")) else {
        return false;
    };
    let file = format!("{id}.jsonl");
    projects
        .flatten()
        .any(|p| p.path().join(&file).is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ah-conv-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ids_are_distinct_v4_uuids() {
        let a = new_id().unwrap();
        let b = new_id().unwrap();
        assert_ne!(a, b);
        let groups: Vec<usize> = a.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12]);
        assert_eq!(&a[14..15], "4");
        assert!("89ab".contains(&a[19..20]));
    }

    #[test]
    fn fill_substitutes_the_placeholder_only() {
        let argv = ["claude", "--resume", "{conversation}"].map(String::from);
        assert!(names_one(&argv));
        assert_eq!(fill(&argv, "abc"), ["claude", "--resume", "abc"]);
        let plain = ["claude", "--continue"].map(String::from);
        assert!(!names_one(&plain));
        assert_eq!(fill(&plain, "abc"), plain);
    }

    #[test]
    fn current_reads_the_live_pid_file() {
        let dir = scratch("current");
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        std::fs::write(
            dir.join("sessions/42.json"),
            r#"{"pid":42,"sessionId":"c49f125b-003c-477b-9a54-a8c9a7382abb","cwd":"/tmp"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("sessions/43.json"), r#"{"sessionId":"../../etc"}"#).unwrap();
        assert_eq!(
            current_in(&dir, 42).as_deref(),
            Some("c49f125b-003c-477b-9a54-a8c9a7382abb")
        );
        assert_eq!(current_in(&dir, 43), None, "not an id we would pass on");
        assert_eq!(current_in(&dir, 44), None, "exited");
    }

    #[test]
    fn a_transcript_is_found_in_any_project() {
        let dir = scratch("transcript");
        std::fs::create_dir_all(dir.join("projects/-tmp-a")).unwrap();
        std::fs::write(dir.join("projects/-tmp-a/abc.jsonl"), "{}\n").unwrap();
        assert!(has_transcript_in(&dir, "abc"));
        assert!(!has_transcript_in(&dir, "def"));
        assert!(!has_transcript_in(&dir.join("nowhere"), "abc"));
    }
}
