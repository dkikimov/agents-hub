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
    assert!(
        out.contains("\"live\":false"),
        "an attach answers with its replay first, even an empty one: {out}"
    );
    let b64 = out.split("\"data\":\"").nth(1).unwrap().split('"').next().unwrap();
    let mut decoded = String::from_utf8_lossy(&base64_decode(b64)).to_string();
    if !decoded.contains("ping-from-pty") {
        decoded += &wait_for_output(&mut r, &id, "ping-from-pty");
    }
    assert!(
        decoded.contains("ping-from-pty"),
        "PTY output should reach the client, got: {decoded:?}"
    );

    // A client that attaches again on the same connection gets one stream, not two:
    // otherwise every byte after it arrives twice.
    send(&mut s, &format!(r#"{{"t":"Attach","id":"{id}","cols":80,"rows":24}}"#));
    wait_for(&mut r, |l| l.contains("\"Output\"") && l.contains("\"live\":false"));
    // "dup-check\n", echoed back by the PTY's line discipline.
    send(&mut s, &format!(r#"{{"t":"Input","id":"{id}","data":"ZHVwLWNoZWNrCg=="}}"#));
    std::thread::sleep(Duration::from_millis(500));
    send(&mut s, r#"{"t":"List"}"#);
    let mut live = String::new();
    loop {
        let mut line = String::new();
        assert!(r.read_line(&mut line).unwrap() > 0, "daemon closed the link");
        if line.contains("\"Sessions\"") {
            break;
        }
        if line.contains("\"Output\"") && line.contains("\"live\":true") {
            let b64 = line.split("\"data\":\"").nth(1).unwrap().split('"').next().unwrap();
            live.push_str(&String::from_utf8_lossy(&base64_decode(b64)));
        }
    }
    assert_eq!(live.matches("dup-check").count(), 1, "a re-attach doubled the stream: {live:?}");

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

    // ── unless the client asks for another ────────────────────────────────────
    send(
        &mut s,
        &format!(r#"{{"t":"Shell","parent":"{parent}","cols":80,"rows":24,"new":true}}"#),
    );
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && sessions(l).len() == 3);
    let second = sessions(&listed)
        .into_iter()
        .find(|(id, p)| *id != shell && p.as_deref() == Some(parent.as_str()))
        .expect("a second shell under the same parent")
        .0;
    send(&mut s, &format!(r#"{{"t":"Kill","id":"{second}"}}"#));
    let listed = wait_for(&mut r, |l| l.contains("\"Sessions\"") && !l.contains(&second));
    assert_eq!(sessions(&listed).len(), 2, "a shell is killed on its own: {listed}");

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

/// The daemon reads an agent's screen and tells every client what it is doing. A
/// stand-in `claude` sets the working spinner title, then finishes its turn and shows
/// the idle title over an empty prompt box.
#[test]
fn the_daemon_reports_what_an_agent_is_doing() {
    let dir = std::env::temp_dir().join(format!("agents-hub-act-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Named `claude`, which is all the daemon goes by. Octal escapes: `⠂` and `✳`.
    let agent = dir.join("claude");
    std::fs::write(
        &agent,
        "#!/bin/sh\n\
         printf '\\033]0;\\342\\240\\202 Fixing it\\007working\\r\\n'\n\
         sleep 4\n\
         printf '\\033]0;\\342\\234\\263 Claude Code\\007\\r\\n\\342\\224\\200\\342\\224\\200\\342\\224\\200\\r\\n\\342\\235\\257 \\r\\n\\342\\224\\200\\342\\224\\200\\342\\224\\200\\r\\n'\n\
         sleep 30\n",
    )
    .unwrap();
    std::fs::set_permissions(&agent, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let cfg = dir.join("config.toml");
    std::fs::write(
        &cfg,
        format!("[[vm]]\nname = \"local\"\n\n[agents.claude]\ncommand = [{:?}]\n", agent),
    )
    .unwrap();

    let _d = start(&dir, &cfg);
    let mut s = connect(&dir);
    let mut r = BufReader::new(s.try_clone().unwrap());
    send(
        &mut s,
        r#"{"t":"Create","agent":"claude","name":"act","cwd":"/tmp","cols":80,"rows":24}"#,
    );
    let first = wait_for(&mut r, |l| l.contains("\"Sessions\"") && l.contains("act"));
    assert!(first.contains("\"activity\":\"Unknown\""), "nothing read yet: {first}");

    // Past the startup grace, the spinner in the title says working…
    wait_for(&mut r, |l| l.contains("\"activity\":\"Working\""));
    // …and the idle title takes over once the turn ends.
    wait_for(&mut r, |l| l.contains("\"activity\":\"Idle\""));

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
