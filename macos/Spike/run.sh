#!/bin/sh
# Builds the spike and wraps it in a minimal .app, because a bare SwiftPM executable
# gets no bundle identity and so never takes keyboard focus — which is most of what
# this spike exists to test. Run it, don't `open` it: stderr is the whole report.
set -e
cd "$(dirname "$0")"
swift build "$@" >&2

APP=.build/Spike.app
mkdir -p "$APP/Contents/MacOS"
cp "$(swift build --show-bin-path "$@")/Spike" "$APP/Contents/MacOS/Spike"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>Spike</string>
  <key>CFBundleIdentifier</key><string>com.agents-hub.spike</string>
  <key>CFBundleName</key><string>agents-hub spike</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1</string>
  <key>LSMinimumSystemVersion</key><string>14.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSPrincipalClass</key><string>NSApplication</string>
</dict></plist>
PLIST
codesign --force --sign - "$APP" 2>/dev/null || true

exec "$APP/Contents/MacOS/Spike"
