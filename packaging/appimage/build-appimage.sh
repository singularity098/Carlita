#!/usr/bin/env bash
# Builds Carlita-<version>-x86_64.AppImage.
#
# Carla is bundled from CARLA_PREFIX (default /usr). For CLAP support build
# Carla >= 2.6 from source, `make install PREFIX=/opt/carla`, and run:
#   CARLA_PREFIX=/opt/carla packaging/appimage/build-appimage.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PREFIX="${CARLA_PREFIX:-/usr}"
APPDIR="$ROOT/packaging/appimage/AppDir"
TOOLS="$ROOT/target/appimage-tools"
VERSION="$(grep -m1 '^version' "$ROOT/Cargo.toml" | cut -d'"' -f2)"

if [ "$PREFIX" != "/usr" ]; then
    export CARLA_INCLUDE_DIR="$PREFIX/include/carla" CARLA_LIB_DIR="$PREFIX/lib/carla"
fi
cargo build --release --manifest-path "$ROOT/Cargo.toml" -p carlita

rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/lib" "$APPDIR/usr/share/carla"
install -m755 "$ROOT/target/release/carlita" "$APPDIR/usr/bin/carlita"
# Carla: engine library, plugin bridges, discovery tools and resources.
cp -a "$PREFIX/lib/carla" "$APPDIR/usr/lib/carla"
cp -a "$PREFIX/share/carla/resources" "$APPDIR/usr/share/carla/" 2>/dev/null || true
install -m755 "$ROOT/packaging/appimage/AppRun" "$APPDIR/AppRun"
install -m644 "$ROOT/packaging/appimage/carlita.desktop" "$APPDIR/carlita.desktop"
install -m644 "$ROOT/assets/branding/carlita-app-icon.svg" "$APPDIR/carlita.svg"

mkdir -p "$TOOLS"
fetch() { [ -x "$TOOLS/$1" ] || { curl -fsSL -o "$TOOLS/$1" "$2"; chmod +x "$TOOLS/$1"; }; }
fetch linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
fetch appimagetool https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-x86_64.AppImage

# Bundle the shared libraries Carlita and Carla depend on (not libjack: the
# host's JACK or PipeWire library must be used).
export APPIMAGE_EXTRACT_AND_RUN=1 NO_STRIP=1
export LD_LIBRARY_PATH="$APPDIR/usr/lib/carla${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
"$TOOLS/linuxdeploy" --appdir "$APPDIR" \
    --executable "$APPDIR/usr/bin/carlita" \
    --library "$APPDIR/usr/lib/carla/libcarla_standalone2.so" \
    --desktop-file "$APPDIR/carlita.desktop" --icon-file "$APPDIR/carlita.svg"
rm -f "$APPDIR"/usr/lib/libjack.so*

# Carla clears LD_LIBRARY_PATH when it starts a plugin bridge, so its own
# programs and libraries must find the bundled libraries (liblo, libmagic,
# fluidsynth...) in usr/lib through their RPATH. Carla ships them with only
# $ORIGIN, which made every bridge fail on machines without those libraries.
find "$APPDIR/usr/lib/carla" -type f | while read -r f; do
    file -b "$f" | grep -q '^ELF' || continue
    rel="$(realpath --relative-to="$(dirname "$f")" "$APPDIR/usr/lib")"
    patchelf --set-rpath "\$ORIGIN:\$ORIGIN/$rel" "$f"
done

mkdir -p "$ROOT/dist"
ARCH=x86_64 "$TOOLS/appimagetool" "$APPDIR" "$ROOT/dist/Carlita-$VERSION-x86_64.AppImage"
echo "Built dist/Carlita-$VERSION-x86_64.AppImage"
