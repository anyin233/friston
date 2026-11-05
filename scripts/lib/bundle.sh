#!/usr/bin/env bash
# Shared .app bundling for dev-app.sh and package-app.sh.
#
# write_bundle APP_PATH BIN BUNDLE_ID NAME ENGINES_DIR
#   Lays out APP_PATH/Contents/{MacOS,Info.plist}: the GUI binary, both Swift engines and
#   their SwiftPM resource bundles (MLX's default.metallib lives in one of them, and the
#   engines look for it next to their executable). TCC permissions belong to the app, so
#   the privacy usage strings live here, not in the engines.

# write_bundle: build the bundle directory; engines are optional (the app explains when missing).
write_bundle() {
  local app="$1" bin="$2" bundle_id="$3" name="$4" engines="$5"
  local macos="$app/Contents/MacOS"
  mkdir -p "$macos"
  cp -f "$bin" "$macos/meeting-master"
  if [[ -d "$engines" ]]; then
    for engine in mm-meetingd mm-embedd; do
      if [[ -x "$engines/$engine" ]]; then
        cp -f "$engines/$engine" "$macos/"
      else
        echo "warning: $engines/$engine missing; run scripts/build-engines.sh" >&2
      fi
    done
    find "$engines" -maxdepth 1 -name '*.bundle' -exec cp -Rf {} "$macos/" \;
  else
    echo "warning: engines folder $engines missing; the app will run without capture/captions/search embeddings" >&2
  fi
  cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleExecutable</key><string>meeting-master</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleName</key><string>$name</string>
  <key>CFBundleDisplayName</key><string>$name</string>
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

# sign_bundle: sign nested executables first, then the app (ad-hoc unless MM_SIGN_IDENTITY is set).
sign_bundle() {
  local app="$1" identity="${MM_SIGN_IDENTITY:--}"
  local f
  for f in "$app/Contents/MacOS/mm-meetingd" "$app/Contents/MacOS/mm-embedd"; do
    [[ -x "$f" ]] && codesign --force --sign "$identity" "$f" >/dev/null
  done
  codesign --force --sign "$identity" "$app" >/dev/null
}
