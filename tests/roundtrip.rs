//! End-to-end: spawn a real session through a real daemon, then prove the promise
//! the design is built on — after a daemon restart the session is still listed,
//! marked Stopped, with its output replayable. The companion shell rides the same
//! machinery, so it is proven the same way.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CONFIG: &str = r#"
[[vm]]
name = "local"

[agents.echo]
command = ["sh", "-c", "echo ping-from-pty; sleep 30"]
"#;

struct Daemon(Child);
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bin() -> PathBuf {
    // target/debug/deps/roundtrip-<hash> → target/debug/agents-hub
    let mut p = std::env::current_exe().unwrap();
    p.pop();
    if p.ends_with("deps") {
        p.pop();
    }
    p.join("agents-hub")
}

fn start(dir: &Path, cfg: &Path) -> Daemon {
    Daemon(
        Command::new(bin())
            .arg("serve")
            .env("AGENTS_HUB_DIR", dir)
            .env("AGENTS_HUB_CONFIG", cfg)
            // Companion shells launch `$SHELL`, whatever the machine running the tests uses.
            .env("SHELL", "/bin/sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon should start — run `cargo build` first"),
    )
}

fn connect(dir: &Path) -> UnixStream {
    let sock = dir.join("sock");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(s) = UnixStream::connect(&sock) {
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            return s;
        }
        assert!(Instant::now() < deadline, "daemon never bound {sock:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn send(s: &mut UnixStream, json: &str) {
    s.write_all(json.as_bytes()).unwrap();
    s.write_all(b"\n").unwrap();
}

/// Reads frames until `pred` matches, so unrelated traffic can't flake the test.
fn wait_for(r: &mut impl BufRead, pred: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "daemon closed the link");
        if pred(&line) {
            return line;
        }
        assert!(Instant::now() < deadline, "timed out waiting for frame");
    }
}

#[test]
fn session_survives_a_daemon_restart() {
    let dir = std::env::temp_dir().join(format!("agents-hub-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, CONFIG).unwrap();

    // ── first daemon: create a session, see its output ────────────────────────
    let d1 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());

    send(
        &mut s,
        r#"{"t":"Create","agent":"echo","name":"smoke","cwd":"/tmp","cols":80,"rows":24}"#,
    );
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && l.contains("smoke"));
    let id = listed
        .split("\"id\":\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("session id in Sessions frame")
        .to_string();
    assert!(listed.contains("\"Running\""), "new session should be Running");

    send(&mut s, &format!(r#"{{"t":"Attach","id":"{id}","cols":80,"rows":24}}"#));
    let out = wait_for(&mut r, |l| l.contains("\"Output\""));
    let b64 = out.split("\"data\":\"").nth(1).unwrap().split('"').next().unwrap();
    let decoded = String::from_utf8_lossy(
        &base64_decode(b64),
    )
    .to_string();
    assert!(
        decoded.contains("ping-from-pty"),
        "PTY output should reach the client, got: {decoded:?}"
    );

    // cwd completion is answered by the machine that would host the session, which is
    // what makes it work for a VM the client can't see the filesystem of
    std::fs::create_dir_all(dir.join("subdir")).unwrap();
    send(&mut s, &format!(r#"{{"t":"ListDir","path":"{}"}}"#, dir.display()));
    let dirs = wait_for(&mut r, |l| l.contains("\"Dirs\""));
    assert!(dirs.contains("\"subdir\""), "expected subdir in {dirs}");
    assert!(!dirs.contains("config.toml"), "files are not completions");

    // metadata hit the disk, not just memory
    let state = std::fs::read_to_string(dir.join("state.json")).unwrap();
    assert!(state.contains(&id) && state.contains("smoke"));

    drop(r);
    drop(s);
    drop(d1);
    std::thread::sleep(Duration::from_millis(300));

    // ── second daemon: same state dir ─────────────────────────────────────────
    let _d2 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());

    send(&mut s, r#"{"t":"List"}"#);
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\""));
    assert!(listed.contains(&id), "session should be restored after restart");
    assert!(
        listed.contains("\"Stopped\""),
        "restored session must be Stopped — its PTY died with the old daemon"
    );

    // scrollback replays from the log, which is what makes `r` useful
    send(&mut s, &format!(r#"{{"t":"Attach","id":"{id}","cols":80,"rows":24}}"#));
    let out = wait_for(&mut r, |l| l.contains("\"Output\""));
    let b64 = out.split("\"data\":\"").nth(1).unwrap().split('"').next().unwrap();
    let replayed = String::from_utf8_lossy(&base64_decode(b64)).to_string();
    assert!(
        replayed.contains("ping-from-pty"),
        "history should replay after restart, got: {replayed:?}"
    );

    // ── kill removes it for good ──────────────────────────────────────────────
    send(&mut s, &format!(r#"{{"t":"Kill","id":"{id}"}}"#));
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && !l.contains(&id));
    assert!(!listed.contains(&id));

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every session in a `Sessions` frame, as (id, parent).
fn sessions(frame: &str) -> Vec<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(frame).unwrap();
    v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let id = s["id"].as_str().unwrap().to_string();
            (id, s["parent"].as_str().map(str::to_string))
        })
        .collect()
}

/// Output of session `id` until it contains `needle`, decoded and concatenated — a
/// shell answers in as many chunks as it likes.
fn wait_for_output(r: &mut impl BufRead, id: &str, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        let frame = wait_for(r, |l| l.contains("\"Output\"") && l.contains(id));
        let b64 = frame.split("\"data\":\"").nth(1).unwrap().split('"').next().unwrap();
        seen.push_str(&String::from_utf8_lossy(&base64_decode(b64)));
    }
    seen
}

#[test]
fn a_companion_shell_lives_and_dies_with_its_session() {
    let dir = std::env::temp_dir().join(format!("agents-hub-shell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, CONFIG).unwrap();

    let d1 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());

    // Canonical because the shell's $PWD is: macOS's temp dir sits behind /var → /private/var.
    let cwd = dir.canonicalize().unwrap().display().to_string();
    send(
        &mut s,
        &format!(r#"{{"t":"Create","agent":"echo","name":"host","cwd":"{cwd}","cols":80,"rows":24}}"#),
    );
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && l.contains("host"));
    let parent = sessions(&listed)[0].0.clone();

    // ── first ask creates it, in the parent's cwd ─────────────────────────────
    let shell_req = format!(r#"{{"t":"Shell","parent":"{parent}","cols":80,"rows":24}}"#);
    send(&mut s, &shell_req);
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && l.contains("\"parent\""));
    let all = sessions(&listed);
    assert_eq!(all.len(), 2, "one agent plus its shell: {listed}");
    let shell = all
        .iter()
        .find(|(_, p)| p.as_deref() == Some(parent.as_str()))
        .expect("the shell names its parent")
        .0
        .clone();

    send(&mut s, &format!(r#"{{"t":"Attach","id":"{shell}","cols":80,"rows":24}}"#));
    // "echo shell-$((40+2)) $PWD\n" — the echo of the typed line never says 42.
    send(
        &mut s,
        &format!(r#"{{"t":"Input","id":"{shell}","data":"ZWNobyBzaGVsbC0kKCg0MCsyKSkgJFBXRAo="}}"#),
    );
    let out = wait_for_output(&mut r, &shell, "shell-42");
    assert!(out.contains(&format!("shell-42 {cwd}")), "ran in the parent's cwd: {out:?}");

    // ── asking again reuses it rather than stacking a second one ──────────────
    send(&mut s, &shell_req);
    send(&mut s, r#"{"t":"List"}"#);
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\""));
    assert_eq!(sessions(&listed).len(), 2, "Shell must be idempotent: {listed}");

    drop(r);
    drop(s);
    drop(d1);
    std::thread::sleep(Duration::from_millis(300));

    // ── after a daemon restart the same ask relaunches it under the same id ───
    let _d2 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());
    send(&mut s, &shell_req);
    let listed = wait_for(&mut r, |l| {
        l.contains("\"Sessions\"") && l.contains(&shell) && l.contains("\"Running\"")
    });
    assert_eq!(sessions(&listed).len(), 2, "relaunched, not recreated: {listed}");

    // ── killing the session takes its shell with it ───────────────────────────
    send(&mut s, &format!(r#"{{"t":"Kill","id":"{parent}"}}"#));
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && !l.contains(&parent));
    assert!(sessions(&listed).is_empty(), "the shell outlived its session: {listed}");
    assert!(!dir.join("logs").join(format!("{shell}.log")).exists());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Two agents that set the window title the way Claude Code does: a bare "Claude Code"
/// first, the real title a beat later. `titled` opts in; `plain` is a `sh`, which the daemon
/// must not treat as an agent that names its work.
const TITLE_CONFIG: &str = r#"
[[vm]]
name = "local"

[agents.titled]
command = ["sh", "-c", '''printf "\033]0;✳ Claude Code\007"; sleep 1; printf "\033]0;✳ Fix login bug\007"; sleep 30''']
title = true

[agents.plain]
command = ["sh", "-c", '''sleep 1; printf "\033]0;user@host: ~/x\007"; sleep 30''']
"#;

/// Every session's name in a `Sessions` frame, sorted.
fn names(frame: &str) -> Vec<String> {
    let v: serde_json::Value = serde_json::from_str(frame).unwrap();
    let mut out: Vec<String> = v["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    out.sort();
    out
}

#[test]
fn an_agents_title_renames_only_the_sessions_nobody_named() {
    let dir = std::env::temp_dir().join(format!("agents-hub-title-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(&cfg, TITLE_CONFIG).unwrap();

    let d1 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());

    let create = |agent: &str, name: &str, auto: bool| {
        format!(
            r#"{{"t":"Create","agent":"{agent}","name":"{name}","cwd":"/tmp","cols":80,"rows":24,"auto":{auto}}}"#
        )
    };
    send(&mut s, &create("titled", "auto-folder", true)); // the folder stood in for a name
    send(&mut s, &create("titled", "chosen", false)); // the user typed this one
    send(&mut s, &create("plain", "a-shell", true)); // auto, but not an agent that titles

    // The generic "Claude Code" title must not rename anything; the real one must.
    let expect = ["Fix login bug", "a-shell", "chosen"];
    let renamed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && l.contains("Fix login bug"));
    assert_eq!(names(&renamed), expect, "in {renamed}");

    // Let `plain` set its title too, then confirm nothing else moved.
    std::thread::sleep(Duration::from_millis(1500));
    send(&mut s, r#"{"t":"List"}"#);
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\""));
    assert_eq!(names(&listed), expect, "in {listed}");

    // The new name is what state.json holds, so it survives a restart.
    let state = std::fs::read_to_string(dir.join("state.json")).unwrap();
    assert!(state.contains("Fix login bug") && !state.contains("auto-folder"));

    drop(r);
    drop(s);
    drop(d1);
    std::thread::sleep(Duration::from_millis(300));

    let _d2 = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());
    send(&mut s, r#"{"t":"List"}"#);
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\""));
    assert_eq!(names(&listed), expect, "after restart: {listed}");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Tiny standalone decoder so the test doesn't depend on the crate's internals.
fn base64_decode(s: &str) -> Vec<u8> {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut acc: u32 = 0;
    let mut bits = 0;
    let mut out = Vec::new();
    for c in s.bytes() {
        let Some(v) = T.iter().position(|&t| t == c) else {
            continue; // '=' padding and any stray whitespace
        };
        acc = (acc << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}
