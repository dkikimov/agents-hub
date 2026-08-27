//! End-to-end: spawn a real session through a real daemon, then prove the promise
//! the design is built on — after a daemon restart the session is still listed,
//! marked Stopped, with its output replayable.

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
