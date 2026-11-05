#!/usr/bin/env bash
# Build, bundle and run Meeting Master as a macOS .app for agent-driven GUI testing.
#
# Why a bundle: Codex Computer Use addresses apps by name, bundle id or .app path and
# refuses bare executables ("Invalid app"). Its per-app approval is keyed to the app, so
# the bundle id and path below must stay stable across rebuilds.
#
# Usage: scripts/dev-app.sh {build|launch|stop|status|path}
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE_ID="${MM_DEV_BUNDLE_ID:-dev.meetingmaster.app.dev}"
APP_NAME="Meeting Master Dev"
BIN_NAME="meeting-master"

# target_dir: print cargo's target directory (honours CARGO_TARGET_DIR and config).
target_dir() {
  if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    echo "$CARGO_TARGET_DIR"
  else
    echo "$REPO_ROOT/target"
  fi
}

APP_PATH="$(target_dir)/debug/MeetingMasterDev.app"

# build: compile the GUI binary and wrap it into a stable, ad-hoc signed .app bundle.
build() {
  (cd "$REPO_ROOT" && cargo build -p mm-app)
  local bin
  bin="$(target_dir)/debug/$BIN_NAME"
  [[ -x "$bin" ]] || { echo "error: expected binary at $bin after build" >&2; exit 1; }

  mkdir -p "$APP_PATH/Contents/MacOS"
  cp "$bin" "$APP_PATH/Contents/MacOS/$BIN_NAME"
  cat >"$APP_PATH/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>$BIN_NAME</string>
  <key>CFBundleIdentifier</key><string>$BUNDLE_ID</string>
  <key>CFBundleName</key><string>$APP_NAME</string>
  <key>CFBundleDisplayName</key><string>$APP_NAME</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
PLIST
  # Ad-hoc signature so macOS treats the bundle as a coherent app (TCC/AX identity).
  codesign --force --sign - "$APP_PATH" >/dev/null
  echo "$APP_PATH"
}

# pids: print PIDs of running dev-bundle instances (empty if none).
pids() {
  pgrep -f "$APP_PATH/Contents/MacOS/$BIN_NAME" || true
}

# stop: terminate running dev instances, escalating to SIGKILL if they ignore SIGTERM.
stop() {
  local p
  p="$(pids)"
  [[ -z "$p" ]] && return 0
  kill $p 2>/dev/null || true
  for _ in {1..20}; do [[ -z "$(pids)" ]] && return 0; sleep 0.1; done
  kill -9 $(pids) 2>/dev/null || true
}

# launch: rebuild, restart the app and wait until its process is up.
launch() {
  build >/dev/null
  stop
  open -n "$APP_PATH"
  for _ in {1..50}; do
    if [[ -n "$(pids)" ]]; then
      echo "launched $APP_NAME ($BUNDLE_ID) pid=$(pids | head -1)"
      return 0
    fi
    sleep 0.2
  done
  echo "error: $APP_NAME did not start within 10s ($APP_PATH)" >&2
  exit 1
}

case "${1:-}" in
  build) build ;;
  launch) launch ;;
  stop) stop ;;
  status) if [[ -n "$(pids)" ]]; then echo "running pid=$(pids | head -1)"; else echo "stopped"; exit 1; fi ;;
  path) echo "$APP_PATH" ;;
  *) echo "usage: $0 {build|launch|stop|status|path}" >&2; exit 2 ;;
esac
