#!/usr/bin/env bash
# Builds the native Swift engines with xcodebuild (plain `swift build` does not compile
# MLX's Metal shaders) and copies the executables + resource bundles to target/engines/.
#
# Usage: scripts/build-engines.sh [Debug|Release] [MeetingEngine|EmbedEngine ...]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CONFIG="${1:-Debug}"
shift || true
PACKAGES=("$@")
[[ ${#PACKAGES[@]} -eq 0 ]] && PACKAGES=(MeetingEngine EmbedEngine)
OUT="$ROOT/target/engines/$CONFIG"
mkdir -p "$OUT"

xcrun metal --version >/dev/null 2>&1 || {
  echo "Metal Toolchain missing. Install it with: xcodebuild -downloadComponent MetalToolchain" >&2
  exit 1
}

for pkg in "${PACKAGES[@]}"; do
  case "$pkg" in
    MeetingEngine) binary=mm-meetingd ;;
    EmbedEngine) binary=mm-embedd ;;
    *) echo "unknown package: $pkg" >&2; exit 2 ;;
  esac
  echo "==> $pkg ($binary, $CONFIG)"
  (
    cd "$ROOT/native/$pkg"
    # Macros (MLXHuggingFace) and package plugins come from pinned dependencies.
    # SwiftPM packages expose one scheme named after the package.
    xcodebuild -scheme "$pkg" -configuration "$CONFIG" \
      -destination 'platform=macOS,arch=arm64' \
      -derivedDataPath .build-xcode \
      -skipMacroValidation -skipPackagePluginValidation \
      build
  )
  products="$ROOT/native/$pkg/.build-xcode/Build/Products/$CONFIG"
  # One folder per engine: the two packages pin different mlx-swift versions, and both ship
  # a resource bundle with the same name, so a shared folder lets one overwrite the other.
  dest="$OUT/$binary"
  rm -rf "$dest" "$OUT/$binary.tmp"
  mkdir -p "$OUT/$binary.tmp"
  cp -f "$products/$binary" "$OUT/$binary.tmp/"
  # SwiftPM resource bundles (model configs, MLX shaders) sit next to the binary.
  find "$products" -maxdepth 1 -name '*.bundle' -exec cp -Rf {} "$OUT/$binary.tmp/" \;
  # MLX looks for a colocated mlx.metallib before any bundle lookup, which keeps the
  # shaders found even when Bundle.main points elsewhere (e.g. inside an .app).
  metallib="$(find "$OUT/$binary.tmp" -path '*mlx-swift_Cmlx.bundle*' -name default.metallib | head -1)"
  [[ -n "$metallib" ]] && cp -f "$metallib" "$OUT/$binary.tmp/mlx.metallib"
  mv "$OUT/$binary.tmp" "$dest"
  echo "    -> $dest/$binary"
done
