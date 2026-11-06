#!/usr/bin/env bash
# Shared .app bundling for dev-app.sh and package-app.sh.
#
# write_bundle APP_PATH BIN BUNDLE_ID NAME ENGINES_DIR
#   Lays out APP_PATH/Contents/{MacOS,Resources/engines,Info.plist}: the GUI binary in MacOS and each
#   Swift engine with its own resource bundles and mlx.metallib in Resources/engines/<engine>/ (SwiftPM resource bundles have no Info.plist, so they must be sealed as resources, not nested code). TCC permissions belong to the app, so
#   the privacy usage strings live here, not in the engines.

# write_bundle: build the bundle directory; engines are optional (the app explains when missing).
write_bundle() {
  local app="$1" bin="$2" bundle_id="$3" name="$4" engines="$5"
  local macos="$app/Contents/MacOS" helpers="$app/Contents/Resources/engines"
  mkdir -p "$macos" "$app/Contents/Resources"
  cp -f "$bin" "$macos/meeting-master"
  # The icon is committed as .icns (regenerate with scripts/make-icon.sh) so bundling needs no rasteriser.
  cp -f "$(dirname "${BASH_SOURCE[0]}")/../../crates/mm-app/assets/Friston.icns" "$app/Contents/Resources/Friston.icns"
  # Contents/Helpers held engines in earlier builds; drop it so stale copies never get signed.
  rm -rf "$helpers" "$app/Contents/Helpers"
  for engine in mm-meetingd mm-embedd; do
    if [[ -x "$engines/$engine/$engine" ]]; then
      # Each engine keeps its own folder: their MLX resource bundles share a name but not a version,
      # and an executable outside Contents/MacOS gets its own folder as Bundle.main.
      mkdir -p "$helpers"
      cp -R "$engines/$engine" "$helpers/"
    else
      echo "warning: $engines/$engine/$engine missing; run scripts/build-engines.sh" >&2
    fi
  done
  cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>meeting-master</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
  <key>CFBundleIconFile</key><string>Friston</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key><string>录制会议声音并生成实时字幕。</string>
  <key>NSCalendarsFullAccessUsageDescription</key><string>读取日程，为每场会议准备笔记和录制设置。</string>
  <key>NSCalendarsUsageDescription</key><string>读取日程，为每场会议准备笔记和录制设置。</string>
  <key>NSScreenCaptureUsageDescription</key><string>录制会议窗口和系统声音。</string>
  <key>NSAudioCaptureUsageDescription</key><string>录制远程参会者的声音。</string>
  <key>CFBundleURLTypes</key>
  <array><dict>
    <key>CFBundleURLName</key><string>$bundle_id.open</string>
    <key>CFBundleURLSchemes</key><array><string>meeting-master</string></array>
  </dict></array>
</dict></plist>
PLIST
}

# signing_identity: print the codesign identity to use.
#   MM_SIGN_IDENTITY wins; otherwise the first "Apple Development" (then "Developer ID
#   Application") identity in the keychain; ad-hoc "-" only as a last resort. A real
#   certificate matters: macOS keys privacy grants (microphone, screen & system audio
#   recording) to the certificate + bundle id, so they survive rebuilds. An ad-hoc
#   signature changes with every build and silently invalidates those grants.
signing_identity() {
  if [[ -n "${MM_SIGN_IDENTITY:-}" ]]; then
    echo "$MM_SIGN_IDENTITY"
    return
  fi
  local hash kind
  for kind in "Apple Development" "Developer ID Application"; do
    hash="$(security find-identity -v -p codesigning 2>/dev/null | awk -v k="\"$kind" 'index($0, k) {print $2; exit}')"
    if [[ -n "$hash" ]]; then
      echo "$hash"
      return
    fi
  done
  echo "warning: no code-signing certificate found; signing ad-hoc, so privacy grants reset on every build" >&2
  echo "-"
}

# sign_bundle: sign nested engine executables first, then the app, with one stable identity.
sign_bundle() {
  local app="$1" identity
  identity="$(signing_identity)"
  local f
  for f in "$app/Contents/Resources/engines/mm-meetingd/mm-meetingd" "$app/Contents/Resources/engines/mm-embedd/mm-embedd"; do
    if [[ -x "$f" ]]; then
      codesign --force --sign "$identity" "$f" >/dev/null || { echo "error: signing $f with $identity failed" >&2; return 1; }
    fi
  done
  codesign --force --sign "$identity" "$app" >/dev/null || { echo "error: signing $app with $identity failed" >&2; return 1; }
}
