#!/bin/sh
# Build a minimal .deb for Ubuntu 24.04 from the release binaries.
# Usage: ./build-deb.sh [version]   (run from native/packaging)
set -eu

VERSION="${1:-0.1.0}"
ROOT="$(cd "$(dirname "$0")" && pwd)"
NATIVE="$(cd "$ROOT/.." && pwd)"
PKG="spacedesk-viewer_${VERSION}_amd64"
STAGE="$ROOT/$PKG"

rm -rf "$STAGE"
mkdir -p "$STAGE/DEBIAN" "$STAGE/usr/bin" "$STAGE/usr/share/applications" "$STAGE/usr/share/icons/hicolor/256x256/apps"

cp "$NATIVE/gui/target/release/spacedesk-viewer" "$STAGE/usr/bin/"
cp "$NATIVE/cli/target/release/spacedesk-cli" "$STAGE/usr/bin/"
cp "$ROOT/spacedesk-viewer.desktop" "$STAGE/usr/share/applications/"
cp "$NATIVE/../icons/spacedesk.png" "$STAGE/usr/share/icons/hicolor/256x256/apps/spacedesk.png"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: spacedesk-viewer
Version: $VERSION
Section: net
Priority: optional
Architecture: amd64
Depends: ffmpeg, libegl1, libgl1, libx11-6, libxkbcommon0, libwayland-client0
Maintainer: 0rmx0
Description: Native high-performance spacedesk viewer for Linux
 WebSocket client for spacedesk Windows servers with VA-API hardware H264
 decoding and OpenGL rendering. Includes a headless CLI (spacedesk-cli).
EOF

chmod 755 "$STAGE/DEBIAN"
dpkg-deb --build "$STAGE" "$ROOT/$PKG.deb"
echo "built $ROOT/$PKG.deb"
