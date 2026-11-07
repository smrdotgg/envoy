#!/usr/bin/env bash
# Builds dist/Envoy.app (optimized, ad-hoc signed). Run from the repo root:
#   scripts/bundle-macos.sh && open dist/Envoy.app
set -euo pipefail

cd "$(dirname "$0")/.."
cargo build --profile dist -p envoy

APP=dist/Envoy.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/dist/envoy "$APP/Contents/MacOS/Envoy"
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Envoy</string>
  <key>CFBundleDisplayName</key><string>Envoy</string>
  <key>CFBundleIdentifier</key><string>dev.envoy.Envoy</string>
  <key>CFBundleExecutable</key><string>Envoy</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.productivity</string>
</dict>
</plist>
PLIST

codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || echo "note: codesign failed; the app still runs locally"
echo "Built $APP"
