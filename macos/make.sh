#!/bin/sh
# Builds AgentsHub.app.
#
# There is no .xcodeproj on purpose: SwiftPM builds this fine, and a hand-assembled
# bundle is a dozen lines of shell that stay readable and diffable, where a generated
# project file is neither. The bundle itself is not optional — a bare SwiftPM executable
# has no bundle identity, and without one the terminal surface never takes keyboard focus.
#
#   ./make.sh                 debug build
#   ./make.sh --release       optimised
#   ./make.sh --release run   build then launch
set -e
cd "$(dirname "$0")"

CONFIG=debug
case "$1" in --release) CONFIG=release; shift ;; esac
SWIFTFLAGS="-c $CONFIG"

echo "→ building the daemon"
CARGOFLAGS=""
[ "$CONFIG" = release ] && CARGOFLAGS="--release"
cargo build $CARGOFLAGS --manifest-path ../Cargo.toml >&2

echo "→ building the app"
swift build $SWIFTFLAGS >&2

APP=".build/AgentsHub.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$(swift build $SWIFTFLAGS --show-bin-path)/AgentsHub" "$APP/Contents/MacOS/AgentsHub"
# Shipped inside the bundle so `Bundle.main.url(forAuxiliaryExecutable:)` finds it: a GUI
# app launched from Finder gets the minimal launchd PATH and would never see ~/.cargo/bin.
# It also makes client/daemon version skew impossible.
cp "../target/$CONFIG/agents-hub" "$APP/Contents/MacOS/agents-hub"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>AgentsHub</string>
  <key>CFBundleIdentifier</key><string>com.agents-hub.app</string>
  <key>CFBundleName</key><string>agents-hub</string>
  <key>CFBundleDisplayName</key><string>agents-hub</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST

# Ad-hoc, and deliberately unsandboxed: the app spawns ssh, which needs ~/.ssh/config,
# the keys and the agent socket — none of which a sandbox container can reach.
codesign --force --sign - "$APP" >/dev/null 2>&1 || true

echo "→ $APP"
[ "$1" = run ] && exec "$APP/Contents/MacOS/AgentsHub"
exit 0
