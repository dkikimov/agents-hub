# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`agents-hub` manages Claude Code / Codex / plain-shell sessions across a local Mac and
remote VMs from one TUI, grouped by VM name. Sessions and their scrollback survive restarts.

## Commands

```bash
cargo build
cargo test                                     # 49 unit + 1 integration
cargo test --test roundtrip                    # integration only (spawns real daemons + PTYs)
cargo test tui::event::                        # one module's tests
cargo test key_bytes_match_a_real_terminal     # single test by name
cargo clippy --all-targets                     # kept clean
cargo install --path . --locked                # --locked matters, see "vt100" below
```

`AGENTS_HUB_DIR` and `AGENTS_HUB_CONFIG` override the state dir and config path. Always set
both when testing so you don't touch the real `~/.local/state/agents-hub/`.

### Driving the TUI

It needs a real pty, so use tmux — `script` gives a 0×0 pty and the app renders *nothing*
with no error, which looks like a hang but isn't:

```bash
tmux new-session -d -s ah -x 100 -y 28 \
  "AGENTS_HUB_DIR=/tmp/ah/state AGENTS_HUB_CONFIG=/tmp/ah/config.toml target/debug/agents-hub"
sleep 3; tmux send-keys -t ah n; sleep 0.5
tmux capture-pane -t ah -p          # the rendered screen, as plain text
tmux kill-session -t ah; pkill -f "agents-hub serve"
```

`capture-pane -p` is the only practical way to see what the UI actually looks like. Sleep
after every `send-keys`; the app coalesces redraws on a 16 ms tick.

## Architecture

One binary, three roles (`src/main.rs` dispatches on `argv[1]`):

| Role | Where | Does |
|---|---|---|
| TUI client (no args) | macOS | `src/tui/` — one task per VM, one vt100 parser per session |
| `serve` | macOS + Linux | `src/server.rs` — owns every PTY, 0600 Unix socket only |
| `stdio` | remote VM | pipes stdin/stdout ↔ local socket; what SSH runs |

`src/setup.rs` is the `install-service` / `add-vm` provisioning, which runs once and
never during normal operation — keep it out of the runtime files.

Inside `src/tui/`, the split is by *who is allowed to mutate what*:

| File | Role |
|---|---|
| `mod.rs` | constants, the `Ui` event union, and the select loop; terminal setup/teardown |
| `app.rs` | all client state, plus the queries and invariants over it (`rebuild`, `reconcile`) |
| `event.rs` | the **only** thing that mutates `App` — keys, mouse, paste, daemon frames |
| `render.rs` | the **only** thing that paints it; writes back just the geometry clicks need |
| `link.rs` | one reconnecting task per VM, local socket or SSH pipe |
| `tree.rs` | cwds → the folder tree, pure |
| `input.rs` | crossterm event → guest bytes, and click → cell hit-testing, pure |
| `clipboard.rs` | OSC 52 from the guest, `pbcopy` locally |

`tree.rs`, `input.rs` and `clipboard.rs` know nothing about `App`, so their tests are
plain function calls. `app.rs::fixture` builds an `App` over a fake VM with the request
channel exposed, which is how `event.rs` asserts on what a keystroke actually sent —
prefer that over driving tmux when the behaviour isn't about pixels.

**Transport is SSH, and there is no network listener anywhere in the binary.** Local clients
hit the Unix socket directly; remote clients spawn `ssh <host> agents-hub stdio`. Both sides
erase to `AsyncRead + AsyncWrite`, so `pump()` handles local and remote identically. Don't add
a TCP/TLS path without a deliberate decision — its absence is the security story, along with
`#![forbid(unsafe_code)]`.

**VM names are client-side only.** The daemon doesn't know what it's called; `[[vm]]` in the
client's config supplies the sidebar grouping. `[agents.*]` is read by the *daemon's* config on
each machine, so a remote VM decides how `claude` launches there. The client's agent-name list
in the "new session" modal comes from the local config and may not match a remote's — a
mismatch surfaces as `Resp::Error` in the status bar.

**Terminal emulation lives on the client.** The server ships raw PTY bytes and appends them to
`logs/<id>.log`; on attach it replays the last 128 KB, then streams. The client feeds all of it
into `vt100::Parser` and renders with `tui-term`. Keep the server dumb about screen state.

**The client attaches to every session and never detaches**, so switching selection is instant
rather than triggering a fresh replay. `App::reconcile` does this; `Ui::Up` clears `attached`
so a reconnect re-attaches and rebuilds parsers from replay.

**Persistence semantics** (deliberate, confirmed with the user): PTYs die with the daemon since
it holds the master fd. `state.json` + per-session logs survive, so after any restart sessions
come back listed as `Stopped` with readable history, and `r` relaunches (using `resume` argv if
the agent defines it). Nothing auto-relaunches. `Hub::new` force-sets every restored session to
`Stopped` — that's not a bug to "fix".

### Locking discipline in server.rs

`Hub::sessions` is a `std::sync::Mutex` and must never be held across an `.await`. The pattern
throughout is: lock, clone the `Arc` you need out, drop the guard, then do the blocking or async
work. `Run`'s fields are `Arc<Mutex<..>>` specifically to make that possible.

## Traps this codebase has already been bitten by

- **`Resp`/`Req` variants must be struct variants.** These are `#[serde(tag = "t")]`
  internally-tagged; serde cannot serialize a newtype variant wrapping a sequence, and
  `to_string` fails at *runtime*, silently dropping the frame. `Resp::Sessions(Vec<_>)` cost an
  afternoon. `proto.rs`'s round-trip test asserts every variant encodes — extend it when adding one.
- **crossterm reports control bytes 0x1c–0x1f as `Ctrl+'4'`–`Ctrl+'7'`**, not `Ctrl+'\'`–`Ctrl+'_'`,
  unless the kitty keyboard protocol is on. So `Ctrl-]` arrives as `Char('5') + CONTROL`. Both
  the escape-key check and `key_bytes()` handle both spellings; the unit test pins it.
- **Never add `vt100` as a direct dependency.** Use `tui_term::vt100`. A separate version pin
  drifts on re-resolve (`cargo install` ignores `Cargo.lock` by default) and yields two vt100
  crates, at which point `PseudoTerminal::new` rejects your `Screen` with a confusing trait error.

## Conventions

- A file earns its existence by having one job someone can name. Split when a file
  stops fitting that sentence, not on a line count; don't add one for a single function.
  Deps are deliberately few — no `clap` (a `match` on argv), no `dirs` (hardcoded
  `~/.config` + `~/.local/state`), no `tracing` (`eprintln!` to the daemon log).
- `ponytail:` comments mark deliberate shortcuts with a named ceiling and upgrade path. Read the
  ceiling before "fixing" the shortcut.
- Non-trivial logic leaves one runnable check behind. `tests/roundtrip.rs` is the load-bearing
  one: create → output → daemon restart → replay → kill, against real PTYs.
