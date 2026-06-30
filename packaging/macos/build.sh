#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VERSION="${VERSION:-0.0.0-dev}"
PKG_VERSION="${PKG_VERSION:-0.0.0}"
ARCH="${ARCH:-$(uname -m)}"
DIST="$ROOT/dist/macos"
WORK="$ROOT/target/macos-package"
APP_NAME="Potty.app"
APP="$WORK/stage/$APP_NAME"

rm -rf "$WORK" "$DIST"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$DIST"

cp "$ROOT/target/release/potty" "$APP/Contents/MacOS/potty"
chmod 0755 "$APP/Contents/MacOS/potty"

if [[ -x "$ROOT/target/release/potty-notify" ]]; then
  mkdir -p "$WORK/stage/bin"
  cp "$ROOT/target/release/potty-notify" "$WORK/stage/bin/potty-notify"
  chmod 0755 "$WORK/stage/bin/potty-notify"
fi

ICONSET="$WORK/potty.iconset"
mkdir -p "$ICONSET"
cp "$ROOT/assets/icon-16.png" "$ICONSET/icon_16x16.png"
cp "$ROOT/assets/icon-32.png" "$ICONSET/icon_16x16@2x.png"
cp "$ROOT/assets/icon-32.png" "$ICONSET/icon_32x32.png"
cp "$ROOT/assets/icon-64.png" "$ICONSET/icon_32x32@2x.png"
cp "$ROOT/assets/icon-128.png" "$ICONSET/icon_128x128.png"
cp "$ROOT/assets/icon-256.png" "$ICONSET/icon_128x128@2x.png"
cp "$ROOT/assets/icon-256.png" "$ICONSET/icon_256x256.png"
cp "$ROOT/assets/icon-512.png" "$ICONSET/icon_256x256@2x.png"
cp "$ROOT/assets/icon-512.png" "$ICONSET/icon_512x512.png"
cp "$ROOT/assets/icon-1024.png" "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/potty.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleDisplayName</key>
  <string>Potty</string>
  <key>CFBundleExecutable</key>
  <string>potty</string>
  <key>CFBundleIconFile</key>
  <string>potty</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.decaychain.potty</string>
  <key>CFBundleName</key>
  <string>Potty</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$PKG_VERSION</string>
  <key>CFBundleVersion</key>
  <string>$PKG_VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSHighResolutionCapable</key>
  <true/>
</dict>
</plist>
EOF

cat > "$WORK/stage/README-macos.txt" <<EOF
Potty $VERSION experimental macOS build

This artifact is unsigned and not notarized yet.

Tarball:
  - Move Potty.app to /Applications if you want a normal app install.
  - Move potty-notify somewhere on PATH if you want Codex/Claude attention-feed hooks.

PKG:
  - Installs Potty.app to /Applications.
  - Installs potty-notify to /usr/local/bin.
EOF

TARBALL="$DIST/potty-$VERSION-macos-$ARCH.tar.gz"
TAR_ITEMS=("$APP_NAME" README-macos.txt)
if [[ -f "$WORK/stage/bin/potty-notify" ]]; then
  TAR_ITEMS+=(bin/potty-notify)
fi
tar -C "$WORK/stage" -czf "$TARBALL" "${TAR_ITEMS[@]}"

APP_ROOT="$WORK/pkg-app-root"
NOTIFY_ROOT="$WORK/pkg-notify-root"
COMPONENTS="$WORK/components"
mkdir -p "$APP_ROOT/Applications" "$NOTIFY_ROOT/usr/local/bin" "$COMPONENTS"
cp -R "$APP" "$APP_ROOT/Applications/$APP_NAME"
if [[ -f "$WORK/stage/bin/potty-notify" ]]; then
  cp "$WORK/stage/bin/potty-notify" "$NOTIFY_ROOT/usr/local/bin/potty-notify"
fi

pkgbuild \
  --root "$APP_ROOT" \
  --identifier io.github.decaychain.potty.app \
  --version "$PKG_VERSION" \
  --install-location / \
  "$COMPONENTS/potty-app.pkg"

PKG_COMPONENTS=(--package "$COMPONENTS/potty-app.pkg")
if [[ -f "$NOTIFY_ROOT/usr/local/bin/potty-notify" ]]; then
  pkgbuild \
    --root "$NOTIFY_ROOT" \
    --identifier io.github.decaychain.potty.notify \
    --version "$PKG_VERSION" \
    --install-location / \
    "$COMPONENTS/potty-notify.pkg"
  PKG_COMPONENTS+=(--package "$COMPONENTS/potty-notify.pkg")
fi

PKG="$DIST/potty-$VERSION-macos-$ARCH.pkg"
productbuild "${PKG_COMPONENTS[@]}" "$PKG"

echo "Built:"
ls -lh "$TARBALL" "$PKG"
