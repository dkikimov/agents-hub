# agents-hub

Claude Code, Codex and plain-shell sessions across your Mac and remote VMs, in one
place. Sessions are grouped by VM and by working directory, and their scrollback
survives restarts.

Two clients, one daemon:

- **TUI** — `agents-hub`, runs in any terminal.
- **AgentsHub.app** — native macOS client with libghostty terminals that use your own
  ghostty config.

There is no network listener. The daemon talks over a `0600` Unix socket, and remote
VMs are reached with `ssh <host> agents-hub stdio`, so access is whatever your SSH
config already allows.

## Install

The [releases page](https://github.com/dkikimov/agents-hub/releases) has `AgentsHub.app`
(Apple Silicon) and the `agents-hub` binary — TUI, daemon and `stdio` in one — for macOS
arm64 and Linux x86_64/arm64. The Linux builds are static, so any distro runs them.

The app is ad-hoc signed, not notarized, so clear the quarantine flag before the first
launch:

```bash
xattr -dr com.apple.quarantine /Applications/AgentsHub.app
```

To build from source you need Rust. The macOS app also needs Swift 6 and macOS 14+.

```bash
make service                 # cargo install --locked, then start the daemon via launchd/systemd
make app                     # optional: build macos/.build/AgentsHub.app
make vm HOST=buildbox        # add a remote: builds it there over SSH and adds it to your config
```

`make vm` bootstraps rustup on the remote if it's missing. Re-run it on a known host to
put that host on the current build.

Run `make` with no target to see everything else.

## Use

```bash
agents-hub                   # the TUI
agents-hub --app gui         # the macOS app
agents-hub open ~/src/foo    # open a folder in the macOS app
```

In the TUI, `?` lists the keys. The ones you need:

| Key | Does |
|---|---|
| `n` | new session (in the selected folder) |
| `⏎` / `l` | focus the terminal |
| `Ctrl-]` | back to the list |
| `Ctrl-]` then `[` | scrollback; `q` / `esc` returns to live |
| `r` | relaunch a stopped session |
| `d` | kill a session |
| `/` | filter |
| `q` | quit |

Click a row to switch sessions, drag the split to resize, drag pane text to copy.

### Restarts

PTYs die with the daemon. The session list and logs don't: after a restart every session
comes back as **Stopped** with its history readable, and `r` relaunches it (using the
agent's `resume` command if it has one). Nothing relaunches on its own.

## Configure

`~/.config/agents-hub/config.toml`, written with defaults on first run:

```toml
[[vm]]
name = "local"                    # group name in the sidebar

[[vm]]
name = "build-box"
ssh  = "buildbox"                 # host alias from ~/.ssh/config
# remote_bin = "agents-hub"       # if it isn't on the remote PATH

[agents.claude]
command = ["claude"]
resume  = ["claude", "--continue"]

[agents.codex]
command = ["codex", "--no-alt-screen"]

[agents.shell]
command = ["$SHELL", "-l"]
```

`[[vm]]` is read by the client. `[agents.*]` is read by the daemon on each machine, so a
remote VM's own config decides how `claude` launches there.

State lives in `~/.local/state/agents-hub/` (socket, `state.json`, per-session logs,
`daemon.log`). `AGENTS_HUB_DIR` and `AGENTS_HUB_CONFIG` override both paths.

## Develop

```bash
make test                    # cargo test + swift test
make lint                    # cargo clippy --all-targets
make run DEBUG=1             # unoptimised build of the app, then launch it
```

Architecture, the protocol contract between the two clients, and the traps this
codebase has already hit are in [CLAUDE.md](CLAUDE.md).
