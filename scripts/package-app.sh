#!/usr/bin/env bash
# Builds a release "Meeting Master.app": optimised GUI binary + Release Swift engines.
#
# Usage: scripts/package-app.sh [--skip-engines]
# Env:   MM_SIGN_IDENTITY  codesign identity (default: ad-hoc "-")
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/bundle.sh
source "$ROOT/scripts/lib/bundle.sh"
APP="$ROOT/target/release/Meeting Master.app"

if [[ "${1:-}" != "--skip-engines" ]]; then
  "$ROOT/scripts/build-engines.sh" Release
fi
(cd "$ROOT" && cargo build --release -p mm-app)
rm -rf "$APP"
write_bundle "$APP" "$ROOT/target/release/meeting-master" "dev.meetingmaster.app" "Meeting Master" "$ROOT/target/engines/Release"
sign_bundle "$APP"
echo "$APP"
