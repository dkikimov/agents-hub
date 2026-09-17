#![forbid(unsafe_code)]

//! Three roles in one binary, chosen by argv: the TUI client (no args), the `serve`
//! daemon that owns the PTYs, and the `stdio` pipe SSH runs on a remote VM.

mod config;
mod proto;
mod server;
mod setup;
mod tui;

use anyhow::{bail, Context, Result};
use config::Config;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::net::UnixStream;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn state_dir() -> PathBuf {
    std::env::var_os("AGENTS_HUB_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state/agents-hub"))
}

pub fn config_path() -> PathBuf {
    std::env::var_os("AGENTS_HUB_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config/agents-hub/config.toml"))
}

pub fn sock_path() -> PathBuf {
    state_dir().join("sock")
}

/// Where `stdio` records the forwarded agent for the daemon to find. Sessions never see
/// this path — they get `agent.sock`, which the daemon proxies onto whatever this points
/// at now, so a session outlives the connection that supplied the key.
pub const AGENT_UPSTREAM: &str = "agent.upstream";

/// The socket the daemon proxies, and the only agent path a session ever sees.
pub const AGENT_PROXY: &str = "agent.sock";

pub fn agent_upstream_path() -> PathBuf {
    state_dir().join(AGENT_UPSTREAM)
}

/// Whether two paths name the same socket, however each is spelled. Compared by identity
/// rather than text because every side of this hands around a different spelling of the
/// same file: a symlink, a `/tmp` that is really `/private/tmp`, a relative `$SSH_AUTH_SOCK`.
/// A path that does not resolve is nobody, so a dangling link never matches.
pub fn same_socket(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
        _ => false,
    }
}

fn relink_agent_sock(sock: &Path, link: &Path) -> Result<()> {
    // A `stdio` started inside a session gets the daemon's own proxy as $SSH_AUTH_SOCK —
    // recording that aims the proxy at its own tail, and the daemon reads the loop back as
    // no agent at all. Pointing the link at itself would ELOOP the same way.
    if sock == link || same_socket(sock, link) || same_socket(sock, &link.with_file_name(AGENT_PROXY))
    {
        return Ok(());
    }
    if let Some(parent) = link.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(link);
    std::os::unix::fs::symlink(sock, link)?;
    Ok(())
}

/// Launches the daemon detached. `ssh host cmd` allocates no TTY, so the daemon
/// gets no SIGHUP on disconnect — nulling its stdio is all the detaching needed.
fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe()?;
    std::fs::create_dir_all(state_dir())?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(state_dir().join("daemon.log"))?;
    std::process::Command::new(exe)
        .arg("serve")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .context("spawning daemon")?;
    Ok(())
}

/// Connects to this machine's daemon, starting it if it isn't up yet.
pub async fn connect_local() -> Result<UnixStream> {
    if let Ok(s) = UnixStream::connect(sock_path()).await {
        return Ok(s);
    }
    spawn_daemon()?;
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if let Ok(s) = UnixStream::connect(sock_path()).await {
            return Ok(s);
        }
    }
    bail!(
        "daemon did not come up; check {}",
        state_dir().join("daemon.log").display()
    )
}

/// What SSH runs on the remote: a dumb pipe between stdin/stdout and the local socket.
/// This process is the only one on the VM that sees the forwarded agent — the daemon
/// was started at boot by systemd — so it re-points the upstream link on every connect.
async fn stdio() -> Result<()> {
    if let Some(sock) = std::env::var_os("SSH_AUTH_SOCK") {
        let _ = relink_agent_sock(Path::new(&sock), &agent_upstream_path());
    }
    let mut sock = connect_local().await?;
    let mut pipe = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    tokio::io::copy_bidirectional(&mut sock, &mut pipe).await?;
    Ok(())
}

const HELP: &str = "\
agents-hub — manage Claude Code / Codex / shell sessions across machines

  agents-hub                       launch the TUI
  agents-hub --app gui             launch the macOS app instead
  agents-hub open [path]           open a folder in the macOS app (default: .)
  agents-hub serve                 run the daemon (owns the PTYs)
  agents-hub stdio                 pipe stdin/stdout to the local daemon (used over SSH)
  agents-hub config --json         print the parsed config (the macOS client reads this)
  agents-hub install-service       install launchd/systemd unit so the daemon survives reboot
  agents-hub install-service --enable
                                    also load/start it now
  agents-hub add-vm <ssh> [name]   install on a remote over SSH and add it to this config
                                    (run from the repo root; bootstraps rustup if needed)
                                    re-run it on a known host to put it on this build

config: ~/.config/agents-hub/config.toml
state:  ~/.local/state/agents-hub/
";

/// `--app tui|gui` wherever it appears, plus the leftover positional. Takes the arguments
/// rather than reading argv so it stays a plain function call under test.
fn app_args<I: Iterator<Item = String>>(
    args: I,
    gui_default: bool,
) -> Result<(bool, Option<String>)> {
    let (mut gui, mut path) = (gui_default, None);
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg == "--app" {
            gui = match args.next().as_deref() {
                Some("gui") => true,
                Some("tui") => false,
                other => bail!("--app takes tui or gui, got '{}'", other.unwrap_or("nothing")),
            };
        } else {
            path = Some(arg);
        }
    }
    Ok((gui, path))
}

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => tui::run().await,
        Some("--app") => {
            let (gui, _) = app_args(std::env::args().skip(1), false)?;
            if gui {
                setup::open_app(None)
            } else {
                tui::run().await
            }
        }
        Some("open") => {
            let (gui, path) = app_args(std::env::args().skip(2), true)?;
            if !gui {
                bail!("open targets the macOS app — a running TUI has no channel to receive a folder");
            }
            setup::open_app(Some(path.as_deref().unwrap_or(".")))
        }
        Some("serve") => {
            let cfg = Config::load(&config_path())?;
            server::serve(state_dir(), cfg).await
        }
        Some("stdio") => stdio().await,
        // So the macOS client doesn't need a TOML parser of its own — one parser stays
        // authoritative, and the client can never disagree with the daemon about a VM.
        Some("config") if std::env::args().nth(2).as_deref() == Some("--json") => {
            let cfg = Config::load(&config_path())?;
            println!("{}", serde_json::to_string(&cfg)?);
            Ok(())
        }
        Some("install-service") => {
            let enable = std::env::args().nth(2).as_deref() == Some("--enable");
            setup::install_service(enable)
        }
        Some("add-vm") => {
            let mut rest = std::env::args().skip(2);
            let host = rest
                .next()
                .context("usage: agents-hub add-vm <ssh-alias> [name]")?;
            setup::add_vm(&host, rest.next().as_deref())
        }
        Some("-h" | "--help" | "help") => {
            print!("{HELP}");
            Ok(())
        }
        Some(other) => bail!("unknown command '{other}'\n\n{HELP}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_link_follows_each_new_connection_and_never_loops() {
        let dir = std::env::temp_dir().join(format!("ah-agent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let link = dir.join(AGENT_UPSTREAM);
        let proxy = dir.join(AGENT_PROXY);

        relink_agent_sock(Path::new("/tmp/ssh-aaa/agent.1"), &link).unwrap();
        relink_agent_sock(Path::new("/tmp/ssh-bbb/agent.2"), &link).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/tmp/ssh-bbb/agent.2")
        );

        // A client run from inside a session: its $SSH_AUTH_SOCK is the daemon's proxy,
        // and it reaches us under whatever name it was given.
        std::fs::File::create(&proxy).unwrap();
        let alias = dir.join("elsewhere.sock");
        std::os::unix::fs::symlink(&proxy, &alias).unwrap();
        relink_agent_sock(&proxy, &link).unwrap();
        relink_agent_sock(&alias, &link).unwrap();
        relink_agent_sock(&link.clone(), &link).unwrap();
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/tmp/ssh-bbb/agent.2"),
            "a session's own SSH_AUTH_SOCK must not replace a live agent with a loop"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_flag_parses() {
        let parse = |argv: &[&str], gui_default| {
            app_args(argv.iter().map(|s| s.to_string()), gui_default)
        };
        assert_eq!(parse(&[], true).unwrap(), (true, None));
        assert_eq!(parse(&["."], true).unwrap(), (true, Some(".".into())));
        assert_eq!(
            parse(&[".", "--app", "tui"], true).unwrap(),
            (false, Some(".".into()))
        );
        assert_eq!(parse(&["--app", "gui"], false).unwrap(), (true, None));
        assert!(parse(&["--app", "nonsense"], false).is_err());
        assert!(parse(&["--app"], false).is_err());
    }
}
