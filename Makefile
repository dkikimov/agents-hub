# agents-hub — one entry point for both clients and the daemon.
#
#   make            what you can build
#   make tui        the Rust binary: TUI client, `serve` daemon and `stdio` in one
#   make app        AgentsHub.app, the native macOS client
#   make test       both suites
#
# DEBUG=1 builds unoptimised, which is what you want while iterating:
#   make app DEBUG=1 run

ifdef DEBUG
  PROFILE     := debug
  CARGO_FLAGS :=
else
  PROFILE     := release
  CARGO_FLAGS := --release
endif

SWIFT_FLAGS := -c $(PROFILE)
RUST_BIN    := target/$(PROFILE)/agents-hub
SWIFT_BIN   := macos/.build/$(PROFILE)/AgentsHub
APP         := macos/.build/AgentsHub.app
ICNS        := macos/.build/AppIcon.icns

.DEFAULT_GOAL := help

.PHONY: help
help:
	@echo 'agents-hub'
	@echo
	@echo '  make tui         Rust binary (TUI client + serve + stdio) -> $(RUST_BIN)'
	@echo '  make app         native macOS client                     -> $(APP)'
	@echo '  make all         both'
	@echo
	@echo '  make run         build and launch the macOS app'
	@echo '  make run-tui     build and launch the TUI'
	@echo
	@echo '  make test        cargo test + swift test'
	@echo '  make lint        cargo clippy --all-targets'
	@echo '  make fmt         cargo fmt'
	@echo
	@echo '  make install     put agents-hub on PATH (cargo install --locked)'
	@echo '  make service     install the launchd/systemd unit and start it'
	@echo '  make vm HOST=x   build and install on a remote over SSH, add it to config'
	@echo
	@echo '  make clean       drop both build trees'
	@echo
	@echo 'DEBUG=1 for an unoptimised build.'

.PHONY: all
all: tui app

# ── the Rust half ────────────────────────────────────────────────────────────

.PHONY: tui
tui: $(RUST_BIN)

# Phony on purpose: cargo already knows what is stale, and teaching make the
# same thing in parallel is how the two end up disagreeing.
.PHONY: $(RUST_BIN)
$(RUST_BIN):
	cargo build $(CARGO_FLAGS)

.PHONY: run-tui
run-tui: tui
	./$(RUST_BIN)

# ── the macOS app ────────────────────────────────────────────────────────────

.PHONY: app
app: $(APP)

.PHONY: $(SWIFT_BIN)
$(SWIFT_BIN):
	cd macos && swift build $(SWIFT_FLAGS)

# Not phony: the icon only changes when its generator does, and rendering ten
# bitmaps on every build is a second nobody asked for.
$(ICNS): macos/icon.swift
	@mkdir -p macos/.build
	@rm -rf macos/.build/AppIcon.iconset
	swift macos/icon.swift macos/.build/AppIcon.iconset
	iconutil -c icns macos/.build/AppIcon.iconset -o $(ICNS)

# The bundle is not optional: a bare SwiftPM executable has no bundle identity,
# and without one the terminal surface never takes keyboard focus.
.PHONY: $(APP)
$(APP): $(SWIFT_BIN) $(RUST_BIN) $(ICNS) macos/Info.plist
	@rm -rf $(APP)
	@mkdir -p $(APP)/Contents/MacOS $(APP)/Contents/Resources
	@cp $(ICNS) $(APP)/Contents/Resources/AppIcon.icns
	@cp $(SWIFT_BIN) $(APP)/Contents/MacOS/AgentsHub
# Shipped inside the bundle so Bundle.main.url(forAuxiliaryExecutable:) finds it:
# a GUI app launched from Finder gets the minimal launchd PATH and would never
# see ~/.cargo/bin. It also makes client/daemon version skew impossible.
	@cp $(RUST_BIN) $(APP)/Contents/MacOS/agents-hub
	@cp macos/Info.plist $(APP)/Contents/Info.plist
# Ad-hoc, and deliberately unsandboxed: the app spawns ssh, which needs
# ~/.ssh/config, the keys and the agent socket, none of which a container reaches.
	@codesign --force --sign - $(APP) >/dev/null 2>&1 || true
	@echo "built $(APP)"

.PHONY: run
run: app
	open $(APP)

# ── checks ───────────────────────────────────────────────────────────────────

.PHONY: test
test:
	cargo test
	cd macos && swift test

.PHONY: lint
lint:
	cargo clippy --all-targets

.PHONY: fmt
fmt:
	cargo fmt

# ── provisioning ─────────────────────────────────────────────────────────────

# --locked matters: cargo install ignores Cargo.lock without it, and a drifting
# vt100 pin yields two vt100 crates and a pane that refuses to render.
.PHONY: install
install:
	cargo install --path . --locked

.PHONY: service
service: install
	agents-hub install-service --enable

.PHONY: vm
vm:
	@test -n "$(HOST)" || { echo 'usage: make vm HOST=<ssh-alias> [NAME=<label>]'; exit 1; }
	cargo run $(CARGO_FLAGS) -- add-vm $(HOST) $(NAME)

.PHONY: clean
clean:
	cargo clean
	rm -rf macos/.build
