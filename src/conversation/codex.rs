use std::fs::{self, DirEntry};
use std::path::{Path, PathBuf};

const TITLE_CONFIG: &str = r#"tui.terminal_title=["spinner","app-name","session-id"]"#;
const UUID_LEN: usize = 36;
// Codex shortens a session ID in the title to 29 characters plus "...".
const TITLE_PREFIX_LEN: usize = 29;

pub struct Command<'a> {
    argv: &'a [String],
    program: usize,
}

impl<'a> Command<'a> {
    pub fn of(argv: &'a [String]) -> Option<Self> {
        let program = if executable(argv.first()?) == Some("env") {
            argv.iter()
                .skip(1)
                .take_while(|arg| arg.contains('='))
                .count()
                + 1
        } else {
            0
        };
        (executable(argv.get(program)?) == Some("codex")).then_some(Self { argv, program })
    }

    pub fn resume(&self, id: &str) -> Vec<String> {
        let mut argv = self.argv.to_vec();
        let args = self.program + 1;
        if !argv[args..]
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .any(|arg| matches!(arg.as_str(), "resume" | "fork" | "agents"))
        {
            argv.splice(args..args, ["resume".into(), id.into()]);
        }
        argv
    }

    pub fn saved_id(&self, cwd: &Path, id: &str) -> Option<String> {
        let home = self.argv[..self.program]
            .iter()
            .rev()
            .find_map(|arg| arg.strip_prefix("CODEX_HOME=").map(PathBuf::from))
            .or_else(|| std::env::var_os("CODEX_HOME").map(PathBuf::from))
            .unwrap_or_else(|| crate::home().join(".codex"));
        saved_id_in(&cwd.join(home), id)
    }
}

pub fn track(mut argv: Vec<String>) -> Vec<String> {
    if let Some(cmd) = Command::of(&argv) {
        let end = argv[cmd.program + 1..]
            .iter()
            .position(|arg| arg == "--")
            .map_or(argv.len(), |i| cmd.program + 1 + i);
        argv.splice(end..end, ["-c".into(), TITLE_CONFIG.into()]);
    }
    argv
}

fn executable(arg: &str) -> Option<&str> {
    Path::new(arg).file_name()?.to_str()
}

pub fn title_id(title: &str) -> Option<&str> {
    let (app, id) = title.rsplit_once(" | ")?;
    if app.split_whitespace().last()? != "codex" {
        return None;
    }
    let id = id.strip_suffix("...").unwrap_or(id);
    valid_id(id).then_some(id)
}

fn valid_id(id: &str) -> bool {
    matches!(id.len(), TITLE_PREFIX_LEN | UUID_LEN)
        && id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
}

fn entries(dir: &Path) -> impl Iterator<Item = DirEntry> {
    fs::read_dir(dir).into_iter().flatten().flatten()
}

fn dirs(dir: &Path) -> impl Iterator<Item = PathBuf> {
    entries(dir)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
}

fn saved_id_in(home: &Path, prefix: &str) -> Option<String> {
    if !valid_id(prefix) {
        return None;
    }
    let mut matches = dirs(&home.join("sessions"))
        .flat_map(|year| dirs(&year))
        .flat_map(|month| dirs(&month))
        .flat_map(|day| entries(&day))
        .filter_map(|entry| {
            let name = entry.file_name();
            let rollout = name.to_str()?.strip_prefix("rollout-")?;
            let stem = rollout
                .strip_suffix(".jsonl.zst")
                .or_else(|| rollout.strip_suffix(".jsonl"))?;
            let id = stem.get(stem.len().checked_sub(UUID_LEN)?..)?;
            (id.starts_with(prefix) && valid_id(id) && entry.file_type().is_ok_and(|t| t.is_file()))
                .then(|| id.to_string())
        });
    let id = matches.next()?;
    matches.all(|other| other == id).then_some(id)
}
