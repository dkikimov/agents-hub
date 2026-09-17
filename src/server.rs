//! The daemon. Owns every PTY on its machine, persists session metadata and output,
//! and speaks `proto` over a 0600 Unix socket. It never opens a network port — remote
//! access arrives via `agents-hub stdio` on the far end of an SSH pipe.

use crate::config::{expand, Config};
use crate::proto::*;
use anyhow::{anyhow, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc};

/// Replayed to a client on attach, so a reconnect shows recent history.
// ponytail: 128 KB per session per attach. With many sessions on a slow link this is
// the startup cost; make it a config knob if it ever bites.
const REPLAY_BYTES: u64 = 128 * 1024;
const LOG_MAX: u64 = 8 * 1024 * 1024;
const LOG_KEEP: u64 = 2 * 1024 * 1024;
const CHUNK_CAP: usize = 1024;

#[derive(Clone, Debug)]
enum Ev {
    Data(Arc<Vec<u8>>),
    Exited(i32),
}

struct Run {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
    tx: broadcast::Sender<Ev>,
}

struct Session {
    info: SessionInfo,
    run: Option<Run>,
}

pub struct Hub {
    dir: PathBuf,
    cfg: Config,
    sessions: Mutex<HashMap<String, Session>>,
    changed: broadcast::Sender<()>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Monotonic-enough unique id without pulling in `uuid` or `rand`.
fn new_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}{:x}", nanos, SEQ.fetch_add(1, Ordering::Relaxed))
}

fn expand_home(cwd: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match (cwd, home) {
        ("~", Some(h)) => h,
        (c, Some(h)) if c.starts_with("~/") => h.join(&c[2..]),
        _ => PathBuf::from(cwd),
    }
}

/// Subdirectory names of `path`, sorted; empty if it isn't a readable directory. The
/// client filters this by what the user has typed, so one listing serves every keystroke.
fn subdirs(path: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(expand_home(path)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn read_tail(path: &Path, max: u64) -> Vec<u8> {
    let Ok(mut f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len > max && f.seek(SeekFrom::Start(len - max)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    buf
}

/// The bytes of `path` that `read_tail(path, tail)` would leave behind.
fn read_head_before(path: &Path, tail: u64) -> Vec<u8> {
    let Ok(f) = std::fs::File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    let _ = f.take(len.saturating_sub(tail)).read_to_end(&mut buf);
    buf
}

/// The DEC private modes the guest turned on or off before the last `tail` bytes, written
/// back out as the escape sequences that restore them.
///
/// A client builds its emulator purely from what we replay, so a mode set once at startup
/// is simply gone the moment the log outgrows the replay window. Bracketed paste is the
/// one that bites: without `ESC[?2004h` the client sends a paste unwrapped, and every
/// newline in it submits the prompt. Alt-screen and focus reporting ride along for free,
/// which is why this restores whatever it finds rather than a list of modes someone has
/// to remember to extend.
fn mode_prelude(path: &Path, tail: u64) -> Vec<u8> {
    let head = read_head_before(path, tail);
    let mut modes: std::collections::BTreeMap<u32, u8> = std::collections::BTreeMap::new();

    let mut i = 0;
    while let Some(off) = head.get(i..).and_then(|r| r.windows(3).position(|w| w == b"\x1b[?")) {
        let params = i + off + 3;
        let mut j = params;
        while matches!(head.get(j), Some(b'0'..=b'9' | b';')) {
            j += 1;
        }
        i = j + 1;
        let Some(&final_byte @ (b'h' | b'l')) = head.get(j) else {
            continue;
        };
        for p in head[params..j].split(|&b| b == b';') {
            if let Some(n) = std::str::from_utf8(p).ok().and_then(|s| s.parse::<u32>().ok()) {
                modes.insert(n, final_byte);
            }
        }
    }

    modes
        .iter()
        .flat_map(|(n, set)| format!("\x1b[?{n}{}", *set as char).into_bytes())
        .collect()
}

/// One session's append-only log, which trims its own head once it outgrows `LOG_MAX`.
/// Every write is best-effort: a full disk must not take the session down with it.
struct SessionLog {
    path: PathBuf,
    file: Option<std::fs::File>,
    since_check: u64,
}

impl SessionLog {
    fn open(path: PathBuf) -> SessionLog {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok();
        SessionLog {
            path,
            file,
            since_check: 0,
        }
    }

    fn append(&mut self, chunk: &[u8]) {
        if let Some(f) = self.file.as_mut() {
            let _ = f.write_all(chunk);
        }
        self.since_check += chunk.len() as u64;
        if self.since_check > LOG_KEEP {
            self.since_check = 0;
            self.file = None; // release the fd before the rename lands on it
            self.trim();
            *self = SessionLog::open(std::mem::take(&mut self.path));
        }
    }

    /// Keeps the tail and drops the head. tmp-file + rename, so a crash mid-trim
    /// leaves the old log rather than a half-written one.
    fn trim(&self) {
        let len = match std::fs::metadata(&self.path) {
            Ok(m) => m.len(),
            Err(_) => return,
        };
        if len <= LOG_MAX {
            return;
        }
        // Modes set before the part we keep would otherwise be unrecoverable — the bytes
        // that carried them are about to be deleted, so `Attach` could never find them.
        let mut tail = mode_prelude(&self.path, LOG_KEEP);
        tail.extend_from_slice(&read_tail(&self.path, LOG_KEEP));
        let tmp = self.path.with_extension("trim");
        if std::fs::write(&tmp, &tail).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

/// Fans one PTY's output out to whoever is attached and appends it to the session log.
/// Returns the child's exit code once the PTY closes.
fn pty_reader(
    mut reader: Box<dyn Read + Send>,
    log_path: PathBuf,
    tx: &broadcast::Sender<Ev>,
    child: &Mutex<Box<dyn Child + Send + Sync>>,
) -> i32 {
    let mut log = SessionLog::open(log_path);
    let mut buf = [0u8; 8192];
    while let Ok(n @ 1..) = reader.read(&mut buf) {
        let chunk = &buf[..n];
        log.append(chunk);
        let _ = tx.send(Ev::Data(Arc::new(chunk.to_vec())));
    }
    child
        .lock()
        .unwrap()
        .wait()
        .map(|s| s.exit_code() as i32)
        .unwrap_or(-1)
}

impl Hub {
    fn new(dir: PathBuf, cfg: Config) -> Hub {
        let restored = Self::load_state(&dir);
        let mut sessions = HashMap::new();
        for mut info in restored {
            // Nothing survives the daemon dying: the PTY master died with it.
            info.status = Status::Stopped;
            sessions.insert(info.id.clone(), Session { info, run: None });
        }
        Hub {
            dir,
            cfg,
            sessions: Mutex::new(sessions),
            changed: broadcast::channel(64).0,
        }
    }

    fn log_path(&self, id: &str) -> PathBuf {
        self.dir.join("logs").join(format!("{id}.log"))
    }

    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    fn load_state(dir: &Path) -> Vec<SessionInfo> {
        std::fs::read_to_string(dir.join("state.json"))
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn list(&self) -> Vec<SessionInfo> {
        let s = self.sessions.lock().unwrap();
        let mut v: Vec<_> = s.values().map(|x| x.info.clone()).collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        v
    }

    /// tmp-file + rename, so a crash mid-write can't leave a truncated state file.
    fn save(&self) {
        let infos = self.list();
        let Ok(json) = serde_json::to_string_pretty(&infos) else {
            return;
        };
        let tmp = self.state_path().with_extension("tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, self.state_path());
        }
    }

    fn announce(&self) {
        self.save();
        let _ = self.changed.send(());
    }

    fn mark_stopped(&self, id: &str) {
        {
            let mut s = self.sessions.lock().unwrap();
            if let Some(sess) = s.get_mut(id) {
                sess.info.status = Status::Stopped;
                sess.run = None;
            }
        }
        self.announce();
    }

    /// Opens a PTY, launches `argv` in `cwd`, and starts the reader thread that
    /// fans output out to attached clients and appends it to the session log.
    fn spawn(self: &Arc<Self>, id: &str, argv: &[String], cwd: &str, cols: u16, rows: u16) -> Result<Run> {
        let argv: Vec<String> = argv.iter().map(|a| expand(a)).collect();
        let (prog, rest) = argv
            .split_first()
            .ok_or_else(|| anyhow!("agent command is empty"))?;

        let pair = native_pty_system().openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })?;

        let mut cmd = CommandBuilder::new(prog);
        cmd.args(rest);
        // `~` means the *daemon's* home, which is the one you want on a remote VM.
        cmd.cwd(expand_home(cwd));
        // Agent CLIs render badly without this.
        cmd.env("TERM", "xterm-256color");
        // The proxy, not the forwarded socket: this path outlives every client, so the
        // env frozen here stays valid across reconnects instead of naming a `/tmp/ssh-*`
        // that dies with the connection that made it. Absent only if the proxy failed
        // to bind, in which case sessions go without rather than get a dead path.
        let agent = self.dir.join(crate::AGENT_PROXY);
        if agent.symlink_metadata().is_ok() {
            cmd.env("SSH_AUTH_SOCK", agent);
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("launching {prog}"))?;
        drop(pair.slave);

        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let tx = broadcast::channel(CHUNK_CAP).0;
        let child = Arc::new(Mutex::new(child));

        let hub = self.clone();
        let sid = id.to_string();
        let log_path = self.log_path(id);
        let etx = tx.clone();
        let echild = child.clone();
        std::thread::spawn(move || {
            let code = pty_reader(reader, log_path, &etx, &echild);
            let _ = etx.send(Ev::Exited(code));
            hub.mark_stopped(&sid);
        });

        Ok(Run {
            writer: Arc::new(Mutex::new(writer)),
            master: Arc::new(Mutex::new(pair.master)),
            child,
            tx,
        })
    }

    fn argv_for(&self, agent: &str, resuming: bool) -> Result<Vec<String>> {
        let a = self
            .cfg
            .agents
            .get(agent)
            .ok_or_else(|| anyhow!("unknown agent type '{agent}' — add [agents.{agent}] to config.toml"))?;
        Ok(match (resuming, &a.resume) {
            (true, Some(r)) => r.clone(),
            _ => a.command.clone(),
        })
    }

    async fn handle(self: &Arc<Self>, req: Req, tx: &mpsc::UnboundedSender<Resp>) -> Result<()> {
        match req {
            Req::List => {
                let _ = tx.send(Resp::Sessions { sessions: self.list() });
            }

            Req::ListDir { path } => {
                let _ = tx.send(Resp::Dirs {
                    names: subdirs(&path),
                    path,
                });
            }

            Req::Create {
                agent,
                name,
                cwd,
                cols,
                rows,
            } => {
                let argv = self.argv_for(&agent, false)?;
                let id = new_id();
                let run = self.spawn(&id, &argv, &cwd, cols, rows)?;
                let info = SessionInfo {
                    id: id.clone(),
                    agent,
                    name,
                    cwd,
                    status: Status::Running,
                    created_at: now_secs(),
                };
                self.sessions
                    .lock()
                    .unwrap()
                    .insert(id, Session { info, run: Some(run) });
                self.announce();
            }

            Req::Restart { id, cols, rows } => {
                let (agent, cwd) = {
                    let s = self.sessions.lock().unwrap();
                    let sess = s.get(&id).ok_or_else(|| anyhow!("no such session"))?;
                    if sess.run.is_some() {
                        return Ok(()); // already running, nothing to do
                    }
                    (sess.info.agent.clone(), sess.info.cwd.clone())
                };
                let argv = self.argv_for(&agent, true)?;
                let run = self.spawn(&id, &argv, &cwd, cols, rows)?;
                {
                    let mut s = self.sessions.lock().unwrap();
                    if let Some(sess) = s.get_mut(&id) {
                        sess.info.status = Status::Running;
                        sess.run = Some(run);
                    }
                }
                self.announce();
            }

            Req::Attach { id, cols, rows } => {
                let mut replay = mode_prelude(&self.log_path(&id), REPLAY_BYTES);
                replay.extend_from_slice(&read_tail(&self.log_path(&id), REPLAY_BYTES));
                if !replay.is_empty() {
                    let _ = tx.send(Resp::Output {
                        id: id.clone(),
                        data: b64(&replay),
                        live: false,
                    });
                }
                let sub = {
                    let s = self.sessions.lock().unwrap();
                    s.get(&id).and_then(|x| x.run.as_ref()).map(|r| {
                        let _ = r.master.lock().unwrap().resize(PtySize {
                            rows: rows.max(1),
                            cols: cols.max(1),
                            pixel_width: 0,
                            pixel_height: 0,
                        });
                        r.tx.subscribe()
                    })
                };
                if let Some(mut sub) = sub {
                    let out = tx.clone();
                    tokio::spawn(async move {
                        loop {
                            match sub.recv().await {
                                Ok(Ev::Data(d)) => {
                                    let msg = Resp::Output {
                                        id: id.clone(),
                                        data: b64(&d),
                                        live: true,
                                    };
                                    if out.send(msg).is_err() {
                                        break;
                                    }
                                }
                                Ok(Ev::Exited(code)) => {
                                    let _ = out.send(Resp::Exited { id, code });
                                    break;
                                }
                                // Dropped chunks garble the pane until the app redraws.
                                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                                Err(_) => break,
                            }
                        }
                    });
                }
            }

            Req::Input { id, data } => {
                let bytes = unb64(&data)?;
                let w = {
                    let s = self.sessions.lock().unwrap();
                    s.get(&id).and_then(|x| x.run.as_ref()).map(|r| r.writer.clone())
                };
                if let Some(w) = w {
                    let mut w = w.lock().unwrap();
                    w.write_all(&bytes)?;
                    w.flush()?;
                }
            }

            Req::Resize { id, cols, rows } => {
                let m = {
                    let s = self.sessions.lock().unwrap();
                    s.get(&id).and_then(|x| x.run.as_ref()).map(|r| r.master.clone())
                };
                if let Some(m) = m {
                    let _ = m.lock().unwrap().resize(PtySize {
                        rows: rows.max(1),
                        cols: cols.max(1),
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }

            Req::Kill { id } => {
                let removed = self.sessions.lock().unwrap().remove(&id);
                if let Some(sess) = removed {
                    if let Some(run) = sess.run {
                        let _ = run.child.lock().unwrap().kill();
                    }
                    let _ = std::fs::remove_file(self.log_path(&id));
                }
                self.announce();
            }
        }
        Ok(())
    }
}

async fn serve_conn(hub: Arc<Hub>, stream: UnixStream) {
    let (r, mut w) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Resp>();

    tokio::spawn(async move {
        while let Some(resp) = rx.recv().await {
            let mut line = match serde_json::to_string(&resp) {
                Ok(l) => l,
                // Unreachable unless a Resp variant stops being encodable; say so
                // rather than dropping the frame into the void.
                Err(e) => {
                    eprintln!("agents-hub: unserializable frame: {e}");
                    continue;
                }
            };
            line.push('\n');
            if w.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });

    // Keep this client's session list fresh when *another* client changes something.
    let mut changed = hub.changed.subscribe();
    let ctx = tx.clone();
    let chub = hub.clone();
    tokio::spawn(async move {
        loop {
            match changed.recv().await {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            }
            if ctx.send(Resp::Sessions { sessions: chub.list() }).is_err() {
                break;
            }
        }
    });

    let mut lines = BufReader::new(r).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Req>(&line) {
            Ok(req) => {
                if let Err(e) = hub.handle(req, &tx).await {
                    // `{e:#}` not `{e}`: the cause is the whole message. A bare
                    // "launching claude" hides the "No such file or directory" that
                    // says it is a PATH problem.
                    let _ = tx.send(Resp::Error { msg: format!("{e:#}") });
                }
            }
            Err(e) => {
                let _ = tx.send(Resp::Error { msg: format!("bad frame: {e}") });
            }
        }
    }
}

/// Where the forwarded agent actually is *right now*: the link the newest `stdio` left,
/// else the daemon's own env, which is what a Mac daemon started from the client inherits.
/// Never the proxy itself — a session's `$SSH_AUTH_SOCK` is the proxy, so a client run from
/// inside one can leave a link pointing at our own tail. Such a link counts as *absent*
/// rather than final: the env behind it is still a real agent, and a daemon that answers
/// nothing takes the keys away from every session at once.
fn current_agent(proxy: &Path, env: Option<PathBuf>) -> Option<PathBuf> {
    let ours = |p: &PathBuf| !crate::same_socket(p, proxy);
    std::fs::read_link(proxy.with_file_name(crate::AGENT_UPSTREAM))
        .ok()
        .filter(ours)
        .or(env)
        .filter(ours)
}

/// The forwarded agent lives at a different `/tmp/ssh-*/agent.<pid>` on every SSH
/// connection and dies with it, so no session can hold that path. The daemon owns
/// `agent.sock` for the life of the box instead and forwards each connection to whatever
/// is live now.
///
/// This cannot conjure an agent while no client is connected — the keys are on the user's
/// Mac and there is no channel to them — and such a connection is simply closed, which
/// the guest reports as "Error connecting to agent". What it does fix is permanence:
/// `Hub::spawn` freezes `$SSH_AUTH_SOCK` for the life of the process, so a session that
/// started while the socket was missing previously had no agent *ever*. Now the path is
/// always there and goes live again on the next client, with no session restart.
async fn agent_proxy(dir: PathBuf) -> Result<()> {
    let sock = dir.join(crate::AGENT_PROXY);
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, PermissionsExt::from_mode(0o600))?;

    loop {
        let (mut client, _) = listener.accept().await?;
        let sock = sock.clone();
        tokio::spawn(async move {
            let env = std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from);
            let Some(path) = current_agent(&sock, env) else { return };
            let Ok(mut up) = UnixStream::connect(&path).await else { return };
            let _ = tokio::io::copy_bidirectional(&mut client, &mut up).await;
        });
    }
}

pub async fn serve(dir: PathBuf, cfg: Config) -> Result<()> {
    std::fs::create_dir_all(dir.join("logs"))?;
    let _ = std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o700));
    let proxy_dir = dir.clone();

    let sock = dir.join("sock");
    if sock.exists() {
        if UnixStream::connect(&sock).await.is_ok() {
            eprintln!("agents-hub: daemon already running at {}", sock.display());
            return Ok(());
        }
        std::fs::remove_file(&sock)?; // stale socket from a dead daemon
    }

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    std::fs::set_permissions(&sock, PermissionsExt::from_mode(0o600))?;
    eprintln!("agents-hub: listening on {}", sock.display());

    // Its own task: a wedged agent must never stall the frame loop, and a bind failure
    // is survivable — `spawn` tests for the socket, so sessions just go without.
    tokio::spawn(async move {
        if let Err(e) = agent_proxy(proxy_dir).await {
            eprintln!("agents-hub: ssh-agent proxy down: {e:#}");
        }
    });

    let hub = Arc::new(Hub::new(dir, cfg));
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve_conn(hub.clone(), stream));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn read_tail_returns_only_the_tail() {
        let dir = std::env::temp_dir().join(format!("ah-tail-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("f");
        std::fs::write(&p, b"0123456789").unwrap();
        assert_eq!(read_tail(&p, 4), b"6789");
        assert_eq!(read_tail(&p, 100), b"0123456789");
        assert!(read_tail(&dir.join("missing"), 10).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_prelude_restores_modes_the_replay_window_cut_off() {
        let dir = std::env::temp_dir().join(format!("ah-modes-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("s.log");

        // Claude Code's real preamble: bracketed paste, alt screen and focus reporting
        // set once at startup, then a megabyte of output nobody replays.
        let head = b"\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?25l\x1b[?25h";
        let mut log = head.to_vec();
        log.extend(std::iter::repeat_n(b'x', 100));
        log.extend_from_slice(b"\x1b[?1000;1006h");
        std::fs::write(&p, &log).unwrap();

        // 1000/1006 are absent because they sit inside the window, where the replay
        // itself carries them; 25 is `h` because the guest's last word wins.
        assert_eq!(mode_prelude(&p, 20), b"\x1b[?25h\x1b[?1004h\x1b[?1049h\x1b[?2004h");

        // Last value wins: a mode the guest turned back off must not come back on.
        std::fs::write(&p, b"\x1b[?2004h\x1b[?2004l").unwrap();
        assert_eq!(mode_prelude(&p, 0), b"\x1b[?2004l");

        // Nothing to restore when the whole log is inside the window.
        assert!(mode_prelude(&p, 1024).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_log_rotates_itself_and_keeps_the_tail() {
        let dir = std::env::temp_dir().join(format!("ah-log-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.log");

        let mut log = SessionLog::open(path.clone());
        let chunk = vec![b'x'; 64 * 1024];
        // Past LOG_MAX, so the check that fires at every LOG_KEEP bytes has to trim.
        for _ in 0..(LOG_MAX / chunk.len() as u64 + 2) {
            log.append(&chunk);
        }
        // The size check only runs every LOG_KEEP bytes, so that overshoot is the
        // real ceiling — LOG_MAX is a rotation trigger, not a hard cap.
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len <= LOG_MAX + LOG_KEEP, "{len} bytes went untrimmed");
        assert!(len >= LOG_KEEP, "the tail is what a client replays from");

        // Appends still land after a rotation — a lost fd would show up as a dead log.
        log.append(b"after");
        assert!(read_tail(&path, 5) == b"after");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Echoes one payload back, standing in for `ssh-agent` on the far end.
    async fn fake_agent(path: PathBuf) {
        let l = UnixListener::bind(&path).unwrap();
        while let Ok((mut c, _)) = l.accept().await {
            tokio::spawn(async move {
                let mut b = [0u8; 64];
                if let Ok(n) = c.read(&mut b).await {
                    let _ = c.write_all(&b[..n]).await;
                }
            });
        }
    }

    async fn dial(sock: &Path) -> Option<UnixStream> {
        for _ in 0..50 {
            if let Ok(s) = UnixStream::connect(sock).await {
                return Some(s);
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        None
    }

    #[tokio::test]
    async fn the_agent_proxy_follows_the_live_socket_and_survives_it_dying() {
        let dir = std::env::temp_dir().join(format!("ah-agent-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (sock, upstream) = (dir.join(crate::AGENT_PROXY), dir.join(crate::AGENT_UPSTREAM));

        // Nothing forwarded yet, and no agent in this process' env either: a session
        // spawned now must still get a path that works *later*.
        std::env::remove_var("SSH_AUTH_SOCK");
        tokio::spawn(agent_proxy(dir.clone()));
        let mut c = dial(&sock).await.expect("proxy binds before any agent exists");
        c.write_all(b"ping").await.unwrap();
        assert_eq!(c.read(&mut [0u8; 4]).await.unwrap(), 0, "no agent yet, so no reply");

        // A client connects: `stdio` points the upstream link at its forwarded socket.
        let first = dir.join("ssh-aaa");
        tokio::spawn(fake_agent(first.clone()));
        dial(&first).await.expect("fake agent came up");
        relink(&first, &upstream);

        let mut c = UnixStream::connect(&sock).await.unwrap();
        c.write_all(b"ping").await.unwrap();
        let mut b = [0u8; 4];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"ping", "the same stable path now reaches a live agent");

        // That connection drops and a new one arrives at a different /tmp path — the
        // case that used to leave every running session signing against a dead socket.
        std::fs::remove_file(&first).unwrap();
        let second = dir.join("ssh-bbb");
        tokio::spawn(fake_agent(second.clone()));
        dial(&second).await.expect("second fake agent came up");
        relink(&second, &upstream);

        let mut c = UnixStream::connect(&sock).await.unwrap();
        c.write_all(b"pong").await.unwrap();
        let mut b = [0u8; 4];
        c.read_exact(&mut b).await.unwrap();
        assert_eq!(&b, b"pong", "sessions follow the new agent without restarting");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn relink(target: &Path, link: &Path) {
        let _ = std::fs::remove_file(link);
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    #[test]
    fn a_link_back_at_our_own_tail_falls_through_to_the_env() {
        let dir = std::env::temp_dir().join(format!("ah-loop-{}", new_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let proxy = dir.join(crate::AGENT_PROXY);
        let real = dir.join("skotty.sock");
        std::fs::File::create(&proxy).unwrap();
        std::fs::File::create(&real).unwrap();

        // What a session sees, so `agents-hub stdio` run *inside* one records this — here
        // under a second name, since the spelling a client passes is not ours to predict.
        let alias = dir.join("elsewhere.sock");
        relink(&proxy, &alias);
        relink(&alias, &dir.join(crate::AGENT_UPSTREAM));

        assert_eq!(
            current_agent(&proxy, Some(real.clone())),
            Some(real),
            "a loop is an absent link, not a dead daemon: the env is still a real agent"
        );
        assert_eq!(
            current_agent(&proxy, Some(alias)),
            None,
            "and with the env pointing at us too there is nothing left to forward to"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ids_are_unique() {
        let a: std::collections::HashSet<_> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(a.len(), 1000);
    }
}
