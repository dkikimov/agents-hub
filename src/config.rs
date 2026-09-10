//! `~/.config/agents-hub/config.toml`. The client reads `[[vm]]`; the daemon on each
//! machine reads `[agents]` to know how to launch things there.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub vm: Vec<Vm>,
    #[serde(default)]
    pub agents: BTreeMap<String, Agent>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Vm {
    /// Display name of the group. Rename freely; nothing else depends on it.
    pub name: String,
    /// SSH host alias from ~/.ssh/config. Absent = this machine.
    #[serde(default)]
    pub ssh: Option<String>,
    #[serde(default = "default_remote_bin")]
    pub remote_bin: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Agent {
    pub command: Vec<String>,
    /// Used by `r` on a stopped session, so Claude Code can pick its thread back up.
    #[serde(default)]
    pub resume: Option<Vec<String>>,
}

fn default_remote_bin() -> String {
    "agents-hub".into()
}

pub const DEFAULT: &str = r#"# agents-hub configuration

[[vm]]
name = "local"                    # group name in the sidebar

# [[vm]]
# name = "build-box"              # group name in the sidebar
# ssh  = "buildbox"               # host alias from ~/.ssh/config
# remote_bin = "agents-hub"       # override if not on the remote PATH

[agents.claude]
command = ["claude"]
resume  = ["claude", "--continue"]

[agents.codex]
command = ["codex", "--no-alt-screen"]

[agents.shell]
command = ["$SHELL", "-l"]
"#;

impl Config {
    /// Reads the config, writing the commented default on first run.
    pub fn load(path: &Path) -> Result<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, DEFAULT)?;
        }
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    pub fn agent_names(&self) -> Vec<String> {
        self.agents.keys().cloned().collect()
    }
}

/// Expands a leading `$VAR` in an argument. Covers `$SHELL`; deliberately not a
/// shell parser.
// ponytail: whole-arg $VAR only. Add real expansion when a config actually needs it.
pub fn expand(arg: &str) -> String {
    match arg.strip_prefix('$') {
        Some(var) => std::env::var(var).unwrap_or_else(|_| arg.to_string()),
        None => arg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_parses() {
        let cfg: Config = toml::from_str(DEFAULT).unwrap();
        assert_eq!(cfg.vm.len(), 1);
        assert_eq!(cfg.vm[0].name, "local");
        assert!(cfg.vm[0].ssh.is_none());
        assert_eq!(cfg.agents["claude"].command, vec!["claude"]);
        assert!(cfg.agents["claude"].resume.is_some());
        assert_eq!(cfg.agents["codex"].command, ["codex", "--no-alt-screen"]);
        assert_eq!(cfg.agent_names(), ["claude", "codex", "shell"]);
    }

    #[test]
    fn remote_vm_defaults_its_binary_name() {
        let cfg: Config =
            toml::from_str("[[vm]]\nname = \"box\"\nssh = \"buildbox\"\n").unwrap();
        assert_eq!(cfg.vm[0].remote_bin, "agents-hub");
    }

    #[test]
    fn expand_handles_var_and_literal() {
        std::env::set_var("AH_TEST_VAR", "/bin/zsh");
        assert_eq!(expand("$AH_TEST_VAR"), "/bin/zsh");
        assert_eq!(expand("-l"), "-l");
        assert_eq!(expand("$AH_NOT_SET_ANYWHERE"), "$AH_NOT_SET_ANYWHERE");
    }
}
