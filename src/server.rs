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
        let tail = read_tail(&self.path, LOG_KEEP);
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
                let replay = read_tail(&self.log_path(&id), REPLAY_BYTES);
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
                    let _ = tx.send(Resp::Error { msg: e.to_string() });
                }
            }
            Err(e) => {
                let _ = tx.send(Resp::Error { msg: format!("bad frame: {e}") });
            }
        }
    }
}

pub async fn serve(dir: PathBuf, cfg: Config) -> Result<()> {
    std::fs::create_dir_all(dir.join("logs"))?;
    let _ = std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o700));

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

    let hub = Arc::new(Hub::new(dir, cfg));
    loop {
        let (stream, _) = listener.accept().await?;
        tokio::spawn(serve_conn(hub.clone(), stream));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn ids_are_unique() {
        let a: std::collections::HashSet<_> = (0..1000).map(|_| new_id()).collect();
        assert_eq!(a.len(), 1000);
    }
}
