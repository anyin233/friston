#!/usr/bin/env bash
# Fetches the sherpa-onnx C API (static, macOS arm64, no TTS) and packages it as
# native/MeetingEngine/Vendor/CSherpaOnnx.xcframework for SwiftPM's binaryTarget(path:).
#
# Why: mm-meetingd uses a streaming Zipformer through sherpa-onnx as the live caption draft
# layer (docs/asr-model-survey.md §9). Upstream ships plain static libraries and no SwiftPM
# package, so we merge them into one library with a module map. Downloads are pinned by
# SHA-256; the result is cached and not committed.
#
# Usage: scripts/fetch-sherpa-onnx.sh [--force]
set -euo pipefail

VERSION="2.0.1"
LIB_SHA256="69413605e7d0d1286d40fbf66e898f7abb53f350d6057cbd154ea2e3f1437d2a"
HEADER_SHA256="5698f38ba09ad2bdf085c56a6cbbd7741acf15e25f4ce5c7097b9702cadf437f"
LIB_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/v$VERSION/sherpa-onnx-v$VERSION-osx-arm64-static-no-tts-lib.tar.bz2"
HEADER_URL="https://raw.githubusercontent.com/k2-fsa/sherpa-onnx/v$VERSION/sherpa-onnx/c-api/c-api.h"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VENDOR="$ROOT/native/MeetingEngine/Vendor"
OUT="$VENDOR/CSherpaOnnx.xcframework"
STAMP="$VENDOR/.sherpa-onnx-version"

if [[ "${1:-}" != "--force" && -d "$OUT" && "$(cat "$STAMP" 2>/dev/null)" == "$VERSION" ]]; then
  exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Downloads $1 to $2 and checks its SHA-256 against $3.
fetch() {
  curl -fsSL --retry 3 -o "$2" "$1"
  local got
  got="$(shasum -a 256 "$2" | cut -d' ' -f1)"
  if [[ "$got" != "$3" ]]; then
    echo "error: checksum mismatch for $1 (got $got, want $3)" >&2
    exit 1
  fi
}

echo "==> sherpa-onnx $VERSION C API"
fetch "$LIB_URL" "$work/lib.tar.bz2" "$LIB_SHA256"
fetch "$HEADER_URL" "$work/c-api.h" "$HEADER_SHA256"
tar -xjf "$work/lib.tar.bz2" -C "$work"
libdir="$work/sherpa-onnx-v$VERSION-osx-arm64-static-no-tts-lib/lib"

# One archive is simpler to ship in an xcframework than eleven. PortAudio (microphone
# capture) and the C++ API are not used: the engine captures audio itself and calls C.
libs=()
for lib in "$libdir"/*.a; do
  case "$(basename "$lib")" in
    libsherpa-onnx-portaudio_static.a|libsherpa-onnx-cxx-api.a) ;;
    *) libs+=("$lib") ;;
  esac
done
libtool -static -no_warning_for_no_symbols -o "$work/libCSherpaOnnx.a" "${libs[@]}"

mkdir -p "$work/include"
cp "$work/c-api.h" "$work/include/"
cat > "$work/include/module.modulemap" <<'EOF'
module CSherpaOnnx {
    header "c-api.h"
    link "c++"
    export *
}
EOF

mkdir -p "$VENDOR"
rm -rf "$OUT"
xcodebuild -create-xcframework -library "$work/libCSherpaOnnx.a" -headers "$work/include" -output "$OUT" >/dev/null
echo "$VERSION" > "$STAMP"
echo "    -> $OUT"
