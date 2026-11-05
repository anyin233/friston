#!/usr/bin/env bash
# Prepares a development machine for Meeting Master. Idempotent; safe to re-run.
#  - verifies Xcode, installs the Metal Toolchain if missing (MLX + GPUI shaders need it)
#  - installs the pinned Rust toolchain (rust-toolchain.toml) and fetches crates
#  - resolves SwiftPM dependencies for the native engines
#  - reports optional integrations (Codex sign-in, Obsidian)
# Model weights are NOT downloaded here; see docs/tech-stack.md.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ok() { printf '  \033[32m✓\033[0m %s\n' "$1"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$1"; }

echo "Xcode"
xcode-select -p >/dev/null 2>&1 || { echo "Xcode is required (xcode-select -p failed)." >&2; exit 1; }
ok "$(xcodebuild -version | head -1)"
if xcrun metal --version >/dev/null 2>&1; then
  ok "Metal Toolchain present"
else
  echo "  installing Metal Toolchain (about 700 MB from Apple)…"
  xcodebuild -downloadComponent MetalToolchain
  ok "Metal Toolchain installed"
fi

echo "Rust"
command -v rustup >/dev/null || { echo "rustup is required: https://rustup.rs" >&2; exit 1; }
(cd "$ROOT" && rustup install >/dev/null 2>&1) # installs the toolchain pinned in rust-toolchain.toml
ok "$(cd "$ROOT" && rustc --version)"
(cd "$ROOT" && cargo fetch --quiet)
ok "crates fetched"

echo "Swift packages"
for pkg in NativeKit MeetingEngine EmbedEngine; do
  (cd "$ROOT/native/$pkg" && xcodebuild -resolvePackageDependencies -quiet >/dev/null 2>&1)
  ok "$pkg resolved"
done

echo "Optional integrations"
# `codex login status` reports on stderr.
if command -v codex >/dev/null && codex login status 2>&1 | grep -qi "logged in"; then
  ok "Codex: $(codex --version 2>/dev/null) ($(codex login status 2>&1 | head -1))"
else
  warn "Codex not installed or not signed in; minutes need BYOK or a local model (run: codex login)"
fi
if [[ -d /Applications/Obsidian.app ]]; then
  obs="$(/usr/libexec/PlistBuddy -c 'Print CFBundleShortVersionString' /Applications/Obsidian.app/Contents/Info.plist)"
  ok "Obsidian installer $obs"
  # The official CLI (optional for us) needs the 1.12+ installer; vault writes do not.
  [[ "$(printf '%s\n1.12\n' "$obs" | sort -V | head -1)" == "1.12" ]] || warn "Obsidian CLI needs installer ≥ 1.12 (optional; Markdown sync works without it)"
else
  warn "Obsidian not installed; notes are still written as Markdown"
fi
echo "Done. Next: make test && make engines && make run"
