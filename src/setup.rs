//! One-time provisioning: the launchd/systemd unit that keeps the daemon alive across
//! reboots, and `add-vm`, which installs this program on a remote over SSH and registers
//! it in the local config. Nothing here runs during normal operation.

use crate::config::Config;
use crate::{config_path, home, state_dir};
use anyhow::{bail, Context, Result};
use std::path::Path;

fn launchd_plist(exe: &str, log: &str) -> String {
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
    )
}

/// Started through a login shell so the daemon sees the same PATH the agents need.
fn systemd_unit(exe: &str, shell: &str) -> String {
    format!(
        "[Unit]\nDescription=agents-hub daemon\n\n\
         [Service]\nExecStart=\"{shell}\" -lc 'exec \"$$0\" serve' \"{exe}\"\nRestart=always\nRestartSec=2\n\n\
         [Install]\nWantedBy=default.target\n"
    )
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("running {program}"))?;
    if !status.success() {
        bail!("{program} {} failed", args.join(" "));
    }
    Ok(())
}

pub fn install_service(enable: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = exe.display().to_string();
    let log = state_dir().join("daemon.log");
    let log = log.display().to_string();

    let (path, body) = if cfg!(target_os = "macos") {
        (
            home().join("Library/LaunchAgents/com.agents-hub.plist"),
            launchd_plist(&exe, &log),
        )
    } else {
        let shell = std::env::var("SHELL").context("$SHELL not set")?;
        (
            home().join(".config/systemd/user/agents-hub.service"),
            systemd_unit(&exe, &shell),
        )
    };
    std::fs::create_dir_all(path.parent().expect("unit path has a parent"))?;
    std::fs::write(&path, body)?;
    println!("wrote {}", path.display());

    match (cfg!(target_os = "macos"), enable) {
        (true, true) => run("launchctl", &["load", "-w", &path.display().to_string()]),
        (true, false) => {
            println!("enable with:  launchctl load -w {}", path.display());
            Ok(())
        }
        (false, true) => {
            run("systemctl", &["--user", "enable", "--now", "agents-hub"])?;
            // Headless VMs kill the user's systemd instance on logout without this.
            let user = std::env::var("USER").context("$USER not set")?;
            run("loginctl", &["enable-linger", &user])
        }
        (false, false) => {
            println!("enable with:  systemctl --user enable --now agents-hub");
            println!("on a headless VM also run:  loginctl enable-linger $USER");
            println!("  (without linger, systemd kills the daemon when you log out)");
            Ok(())
        }
    }
}

/// The `[[vm]]` block appended to the local config by `add-vm`.
fn vm_toml_block(name: &str, host: &str) -> String {
    format!("\n[[vm]]\nname = \"{name}\"\nssh  = \"{host}\"\n")
}

/// `ssh host agents-hub stdio` (how the client reconnects later) runs a non-interactive
/// shell, which skips .bashrc/.profile and so never sees ~/.cargo/bin. Symlink onto a
/// directory that's on the default PATH so that works without `remote_bin` in config.toml.
fn remote_install_script(dir: &str) -> String {
    format!(
        "source ~/.cargo/env 2>/dev/null; \
         command -v cargo >/dev/null 2>&1 || curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y; \
         source ~/.cargo/env && \
         cd {dir} && cargo install --path . --locked && agents-hub install-service --enable && \
         (sudo -n ln -sf ~/.cargo/bin/agents-hub /usr/local/bin/agents-hub 2>/dev/null || \
          echo 'warn: could not symlink agents-hub onto the default PATH (no passwordless sudo?); set remote_bin to an absolute path in config.toml instead' >&2)"
    )
}

/// SSHes to `host`, bootstraps rustup if cargo isn't there, builds this checkout,
/// installs it as a service, and registers it in the local config.
pub fn add_vm(host: &str, name: Option<&str>) -> Result<()> {
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
    run(
        "rsync",
        &[
            "-a",
            "--exclude-from=.gitignore",
            "--exclude",
            ".git",
            "./",
            &format!("{host}:{remote_dir}/"),
        ],
    )?;

    println!("→ building and installing the service on {host}");
    run("ssh", &[host, &remote_install_script(remote_dir)])?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_block_appends_onto_default_config_and_parses() {
        let mut text = crate::config::DEFAULT.to_string();
        text.push_str(&vm_toml_block("box", "buildbox"));
        let cfg: Config = toml::from_str(&text).unwrap();
        assert!(cfg
            .vm
            .iter()
            .any(|v| v.name == "box" && v.ssh.as_deref() == Some("buildbox")));
    }

    #[test]
    fn linux_service_starts_daemon_from_login_shell() {
        let unit = systemd_unit("/home/u/.cargo/bin/agents-hub", "/bin/bash");
        assert!(unit.contains(
            "ExecStart=\"/bin/bash\" -lc 'exec \"$$0\" serve' \"/home/u/.cargo/bin/agents-hub\"\n"
        ));
    }

    #[test]
    fn macos_service_names_the_binary_and_its_log() {
        let plist = launchd_plist("/opt/bin/agents-hub", "/tmp/daemon.log");
        assert!(plist.contains("<string>/opt/bin/agents-hub</string><string>serve</string>"));
        assert!(plist.contains("<key>StandardErrorPath</key><string>/tmp/daemon.log</string>"));
    }

    #[test]
    fn remote_install_bootstraps_rust_before_building() {
        let s = remote_install_script("agents-hub-src");
        let (rustup, build) = (
            s.find("sh.rustup.rs").unwrap(),
            s.find("cargo install").unwrap(),
        );
        assert!(rustup < build, "rustup must be in place before the build");
        assert!(s.contains("--locked"), "a drifting vt100 pin breaks the pane");
    }
}
