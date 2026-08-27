#![forbid(unsafe_code)]

mod config;
mod proto;
mod server;
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

fn install_service(enable: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.display().to_string();
    let log = state_dir().join("daemon.log");
    let log = log.display().to_string();

    if cfg!(target_os = "macos") {
        let path = home().join("Library/LaunchAgents/com.agents-hub.plist");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>Label</key><string>com.agents-hub</string>
  <key>ProgramArguments</key><array><string>{exe}</string><string>serve</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardErrorPath</key><string>{log}</string>
</dict></plist>
"#
            ),
        )?;
        println!("wrote {}", path.display());
        if enable {
            let status = std::process::Command::new("launchctl")
                .args(["load", "-w"])
                .arg(&path)
                .status()
                .context("running launchctl")?;
            if !status.success() {
                bail!("launchctl load failed");
            }
        } else {
            println!("enable with:  launchctl load -w {}", path.display());
        }
    } else {
        let path = home().join(".config/systemd/user/agents-hub.service");
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            format!(
                "[Unit]\nDescription=agents-hub daemon\n\n\
                 [Service]\nExecStart={exe} serve\nRestart=always\nRestartSec=2\n\n\
                 [Install]\nWantedBy=default.target\n"
            ),
        )?;
        println!("wrote {}", path.display());
        if enable {
            let status = std::process::Command::new("systemctl")
                .args(["--user", "enable", "--now", "agents-hub"])
                .status()
                .context("running systemctl")?;
            if !status.success() {
                bail!("systemctl enable failed");
            }
            // Headless VMs kill the user's systemd instance on logout without this.
            let user = std::env::var("USER").context("$USER not set")?;
            std::process::Command::new("loginctl")
                .args(["enable-linger", &user])
                .status()
                .context("running loginctl")?;
        } else {
            println!("enable with:  systemctl --user enable --now agents-hub");
            println!("on a headless VM also run:  loginctl enable-linger $USER");
            println!("  (without linger, systemd kills the daemon when you log out)");
        }
    }
    Ok(())
}

/// The `[[vm]]` block appended to the local config by `add-vm`.
fn vm_toml_block(name: &str, host: &str) -> String {
    format!("\n[[vm]]\nname = \"{name}\"\nssh  = \"{host}\"\n")
}

/// SSHes to `host`, bootstraps rustup if cargo isn't there, builds this checkout,
/// installs it as a service, and registers it in the local config.
fn add_vm(host: &str, name: Option<&str>) -> Result<()> {
    if !Path::new("Cargo.toml").exists() {
        bail!("run this from the agents-hub repo root (no Cargo.toml in the current directory)");
    }
    let name = name.unwrap_or(host);

    let cfg = Config::load(&config_path())?;
    if cfg.vm.iter().any(|v| v.ssh.as_deref() == Some(host)) {
        bail!("'{host}' is already configured in {}", config_path().display());
    }

    let remote_dir = "agents-hub-src";
    println!("→ copying source to {host}:~/{remote_dir}/");
    let status = std::process::Command::new("rsync")
        .args(["-a", "--exclude-from=.gitignore", "--exclude", ".git", "./"])
        .arg(format!("{host}:{remote_dir}/"))
        .status()
        .context("running rsync")?;
    if !status.success() {
        bail!("rsync to {host} failed");
    }

    println!("→ building and installing the service on {host}");
    // `ssh host agents-hub stdio` (how the client reconnects later) runs a
    // non-interactive shell, which skips .bashrc/.profile and so never sees
    // ~/.cargo/bin. Symlink onto a directory that's on the default PATH so
    // that works without needing `remote_bin` set in config.toml.
    let status = std::process::Command::new("ssh")
        .arg(host)
        .arg(format!(
            "source ~/.cargo/env 2>/dev/null; \
             command -v cargo >/dev/null 2>&1 || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; \
             source ~/.cargo/env && \
             cd {remote_dir} && cargo install --path . --locked && agents-hub install-service --enable && \
             (sudo -n ln -sf ~/.cargo/bin/agents-hub /usr/local/bin/agents-hub 2>/dev/null || \
              echo 'warn: could not symlink agents-hub onto the default PATH (no passwordless sudo?); set remote_bin to an absolute path in config.toml instead' >&2)"
        ))
        .status()
        .context("running ssh")?;
    if !status.success() {
        bail!("remote install on {host} failed");
    }

    let path = config_path();
    let mut text = std::fs::read_to_string(&path)?;
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&vm_toml_block(name, host));
    std::fs::write(&path, text)?;
    println!("→ added [[vm]] \"{name}\" to {}", path.display());
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
            install_service(enable)
        }
        Some("add-vm") => {
            let mut rest = std::env::args().skip(2);
            let host = rest
                .next()
                .context("usage: agents-hub add-vm <ssh-alias> [name]")?;
            add_vm(&host, rest.next().as_deref())
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
    fn vm_block_appends_onto_default_config_and_parses() {
        let mut text = config::DEFAULT.to_string();
        text.push_str(&vm_toml_block("box", "buildbox"));
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg
            .vm
            .iter()
            .any(|v| v.name == "box" && v.ssh.as_deref() == Some("buildbox")));
    }
}
