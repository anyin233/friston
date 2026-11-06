#!/usr/bin/env bash
# Build, bundle and run Friston as a macOS .app for agent-driven GUI testing.
#
# Why a bundle: Codex Computer Use addresses apps by name, bundle id or .app path and
# refuses bare executables ("Invalid app"). Its per-app approval is keyed to the app, so
# the bundle id and path below must stay stable across rebuilds.
#
# Signing: TCC keys grants to the code signature's designated requirement. An ad-hoc signature
# pins that to the cdhash, so every rebuild looks like a new app and loses microphone/screen
# grants. We therefore sign with the first "Apple Development" identity in the keychain, whose
# requirement (bundle id + certificate) survives rebuilds. MM_SIGN_IDENTITY overrides this;
# set it to "-" to force ad-hoc.
#
# Usage: scripts/dev-app.sh {build|launch|stop|status|path|sign-identity}
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUNDLE_ID="${MM_DEV_BUNDLE_ID:-dev.friston.app.dev}"
APP_NAME="Friston Dev"
BIN_NAME="meeting-master"

# target_dir: print cargo's target directory (honours CARGO_TARGET_DIR and config).
target_dir() {
  if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    echo "$CARGO_TARGET_DIR"
  else
    echo "$REPO_ROOT/target"
  fi
}

APP_PATH="$(target_dir)/debug/FristonDev.app"
# Bundle from before the Friston rename; removed on build so it cannot claim the URL schemes.
LEGACY_APP_PATH="$(target_dir)/debug/MeetingMasterDev.app"
# shellcheck source=lib/bundle.sh
source "$REPO_ROOT/scripts/lib/bundle.sh"

# sign_identity: print the codesign identity for the dev bundle.
#   Order: $MM_SIGN_IDENTITY, else the SHA-1 of the first "Apple Development" identity (SHA-1, not
#   the name, so duplicate certificates with the same name stay unambiguous), else "-" (ad-hoc).
sign_identity() {
  if [[ -n "${MM_SIGN_IDENTITY:-}" ]]; then
    echo "$MM_SIGN_IDENTITY"
    return
  fi
  local sha
  sha="$(security find-identity -v -p codesigning 2>/dev/null \
    | awk '/"Apple Development: /{print $2; exit}')"
  echo "${sha:--}"
}

# build: compile the GUI binary and wrap it into a .app bundle signed with a stable identity.
build() {
  (cd "$REPO_ROOT" && cargo build -p mm-app)
  local bin
  bin="$(target_dir)/debug/$BIN_NAME"
  [[ -x "$bin" ]] || { echo "error: expected binary at $bin after build" >&2; exit 1; }

  rm -rf "$LEGACY_APP_PATH"
  write_bundle "$APP_PATH" "$bin" "$BUNDLE_ID" "$APP_NAME" "${MM_ENGINES_DIR:-$REPO_ROOT/target/engines/Debug}"
  local identity
  identity="$(sign_identity)"
  if [[ "$identity" == "-" ]]; then
    echo "warning: no Apple Development identity; ad-hoc signing, TCC grants reset on every rebuild" >&2
  fi
  MM_SIGN_IDENTITY="$identity" sign_bundle "$APP_PATH"
  echo "$APP_PATH"
}

# pids: print PIDs of running dev-bundle instances (empty if none), including a pre-rename build
#   still running from the legacy path, so `launch` never leaves two copies fighting over the data folder.
pids() {
  pgrep -f "$APP_PATH/Contents/MacOS/$BIN_NAME" || true
  pgrep -f "$LEGACY_APP_PATH/Contents/MacOS/$BIN_NAME" || true
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
  sign-identity) sign_identity ;;
  *) echo "usage: $0 {build|launch|stop|status|path|sign-identity}" >&2; exit 2 ;;
esac
