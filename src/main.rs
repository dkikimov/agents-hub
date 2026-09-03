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
use std::path::PathBuf;
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
async fn stdio() -> Result<()> {
    let mut sock = connect_local().await?;
    let mut pipe = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    tokio::io::copy_bidirectional(&mut sock, &mut pipe).await?;
    Ok(())
}

const HELP: &str = "\
agents-hub — manage Claude Code / Codex / shell sessions across machines

  agents-hub                       launch the TUI
  agents-hub serve                 run the daemon (owns the PTYs)
  agents-hub stdio                 pipe stdin/stdout to the local daemon (used over SSH)
  agents-hub install-service       install launchd/systemd unit so the daemon survives reboot
  agents-hub install-service --enable
                                    also load/start it now
  agents-hub add-vm <ssh> [name]   install on a remote over SSH and add it to this config
                                    (run from the repo root; bootstraps rustup if needed)
                                    re-run it on a known host to put it on this build

config: ~/.config/agents-hub/config.toml
state:  ~/.local/state/agents-hub/
";

#[tokio::main]
async fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        None => tui::run().await,
        Some("serve") => {
            let cfg = Config::load(&config_path())?;
            server::serve(state_dir(), cfg).await
        }
        Some("stdio") => stdio().await,
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
