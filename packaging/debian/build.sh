#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
if [[ -n "${VERSION:-}" ]]; then
  VERSION_RAW="$VERSION"
elif [[ -n "${GITHUB_REF_NAME:-}" ]]; then
  VERSION_RAW="${GITHUB_REF_NAME#v}"
else
  VERSION_RAW=""
fi
DEB_VERSION="${DEB_VERSION:-${VERSION_RAW//-/~}}"
DEB_ARCH="${DEB_ARCH:-amd64}"
DIST="$ROOT/dist/debian"

if [[ -z "$VERSION_RAW" || "$VERSION_RAW" == "refs/"* ]]; then
  echo "VERSION or GITHUB_REF_NAME must name the release version" >&2
  exit 2
fi

if [[ "$DEB_ARCH" != "amd64" ]]; then
  echo "Only amd64 Debian packages are supported for now (got $DEB_ARCH)" >&2
  exit 2
fi

case "$(uname -m)" in
  x86_64 | amd64) ;;
  *)
    echo "Ubuntu packages are built only on x86_64 runners for now" >&2
    exit 2
    ;;
esac

command -v dpkg-deb >/dev/null || {
  echo "dpkg-deb is required to build Debian packages" >&2
  exit 2
}

for bin in potty potty-session potty-notify; do
  if [[ ! -x "$ROOT/target/release/$bin" ]]; then
    echo "missing target/release/$bin; run cargo build --release first" >&2
    exit 2
  fi
done

rm -rf "$DIST"
mkdir -p "$DIST"

write_copyright() {
  local docdir="$1"
  install -d "$docdir"
  {
    echo "potty is distributed under the MIT license."
    echo
    cat "$ROOT/LICENSE"
  } > "$docdir/copyright"
}

build_potty() {
  local root="$DIST/root-potty"
  rm -rf "$root"

  install -Dm755 "$ROOT/target/release/potty" "$root/usr/bin/potty"
  install -Dm644 "$ROOT/packaging/fedora/io.github.decaychain.potty.desktop" \
    "$root/usr/share/applications/io.github.decaychain.potty.desktop"
  install -Dm644 "$ROOT/assets/icon.svg" \
    "$root/usr/share/icons/hicolor/scalable/apps/io.github.decaychain.potty.svg"
  for size in 48 64 128 256; do
    install -Dm644 "$ROOT/assets/icon-${size}.png" \
      "$root/usr/share/icons/hicolor/${size}x${size}/apps/io.github.decaychain.potty.png"
  done
  write_copyright "$root/usr/share/doc/potty"

  install -d "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<EOF
Package: potty
Version: ${DEB_VERSION}
Section: x11
Priority: optional
Architecture: ${DEB_ARCH}
Maintainer: Decay Chain <noreply@github.com>
Homepage: https://github.com/decaychain/potty
Depends: libc6 (>= 2.39), libgcc-s1 (>= 4.2), libwayland-client0, libwayland-cursor0, libwayland-egl1, libxkbcommon0, libvulkan1, fonts-dejavu-core
Recommends: potty-tools (= ${DEB_VERSION})
Description: GPU-accelerated terminal emulator with visual tabs and panes
 potty is a GPU-accelerated terminal emulator in Rust with a deliberately
 visual, pointer-driven take on tabs and panes. It is Wayland-native, with a
 custom per-cell GPU renderer and real multiplexing.
EOF

  dpkg-deb --build --root-owner-group "$root" "$DIST/potty_${DEB_VERSION}_${DEB_ARCH}.deb"
}

build_tools() {
  local root="$DIST/root-potty-tools"
  rm -rf "$root"

  install -Dm755 "$ROOT/target/release/potty-session" "$root/usr/bin/potty-session"
  install -Dm755 "$ROOT/target/release/potty-notify" "$root/usr/bin/potty-notify"
  write_copyright "$root/usr/share/doc/potty-tools"

  install -d "$root/DEBIAN"
  cat > "$root/DEBIAN/control" <<EOF
Package: potty-tools
Version: ${DEB_VERSION}
Section: utils
Priority: optional
Architecture: ${DEB_ARCH}
Maintainer: Decay Chain <noreply@github.com>
Homepage: https://github.com/decaychain/potty
Depends: libc6 (>= 2.39), libgcc-s1 (>= 4.2)
Description: Remote-session and notification helpers for potty
 Helper programs for potty that carry none of the GUI dependencies:
 potty-session is the SSH persistence daemon, and potty-notify is the
 attention-feed helper used by agentic CLI hooks.
EOF

  dpkg-deb --build --root-owner-group "$root" "$DIST/potty-tools_${DEB_VERSION}_${DEB_ARCH}.deb"
}

build_potty
build_tools

ls -lh "$DIST"/*.deb
