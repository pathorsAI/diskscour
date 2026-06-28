#!/usr/bin/env bash
# Build DiskScour, assemble DiskScour.app, and package it as a distributable zip.
#
#   ./scripts/bundle-mac.sh            # fast native build (current arch)
#   UNIVERSAL=1 ./scripts/bundle-mac.sh   # universal arm64 + x86_64 (used by CI)
#
# Outputs: dist/DiskScour.app and dist/DiskScour-v<version>-macos.zip
set -euo pipefail
cd "$(dirname "$0")/.."

VERSION="$(grep -m1 '^version[[:space:]]*=' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
APP="dist/DiskScour.app"
mkdir -p dist

if [[ "${UNIVERSAL:-0}" == "1" ]]; then
  echo "Building universal (arm64 + x86_64) DiskScour v$VERSION ..."
  rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  cargo build --release --target aarch64-apple-darwin
  cargo build --release --target x86_64-apple-darwin
  BIN="dist/diskscour"
  lipo -create -output "$BIN" \
    target/aarch64-apple-darwin/release/diskscour \
    target/x86_64-apple-darwin/release/diskscour
else
  echo "Building native DiskScour v$VERSION ..."
  cargo build --release
  BIN="target/release/diskscour"
fi

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/diskscour"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>DiskScour</string>
  <key>CFBundleDisplayName</key><string>DiskScour</string>
  <key>CFBundleExecutable</key><string>diskscour</string>
  <key>CFBundleIdentifier</key><string>com.pathors.diskscour</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>LSMinimumSystemVersion</key><string>10.15</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
</dict>
</plist>
PLIST

# Ad-hoc signature so the bundle is internally consistent (not notarized).
codesign --force --deep --sign - "$APP" >/dev/null 2>&1 || true

ZIP="dist/DiskScour-v${VERSION}-macos.zip"
rm -f "$ZIP"
ditto -c -k --keepParent "$APP" "$ZIP"

echo "Built     $APP"
echo "Packaged  $ZIP"
