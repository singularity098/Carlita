#!/usr/bin/env bash
# Builds Carlita-<version>-x86_64.AppImage.
#
# Carla is built from source by packaging/build-carla.sh (a pinned commit of
# its main branch, with CLAP and native VST3, and Carlita's patches). To
# bundle a Carla installed elsewhere instead, set CARLA_PREFIX, e.g.
#   CARLA_PREFIX=/usr packaging/appimage/build-appimage.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PREFIX="${CARLA_PREFIX:-$("$ROOT/packaging/build-carla.sh")}"
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

# A Carla from CARLA_PREFIX lacks Carlita's patches (see carla-patches/):
# rebuild its plugin bridge from the same Carla version with them. Without
# them, editors of plugins running in their own process are always drawn at
# 1.0. Set CARLITA_STOCK_BRIDGE=1 to keep Carla's own bridge.
if [ -n "${CARLA_PREFIX:-}" ] && [ -z "${CARLITA_STOCK_BRIDGE:-}" ]; then
    CARLA_VERSION="$(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" \
        pkg-config --modversion carla-standalone)"
    SRC="$ROOT/target/carla-src-$CARLA_VERSION"
    if [ ! -x "$SRC/bin/carla-bridge-native" ]; then
        rm -rf "$SRC"
        git clone --quiet --depth 1 --branch "v$CARLA_VERSION" https://github.com/falkTX/Carla.git "$SRC"
        for patch in "$ROOT"/packaging/appimage/carla-patches/*.patch; do
            git -C "$SRC" apply "$patch"
        done
        make -C "$SRC" -j"$(nproc)" bridges-plugin
    fi
    install -m755 "$SRC/bin/carla-bridge-native" "$APPDIR/usr/lib/carla/carla-bridge-native"
fi
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
# Nor libxkbcommon: Carlita's window library loads it together with the
# host's libxkbcommon-x11, and the two must be the same version. With an
# older bundled one, typing on a newer system crashed Carlita.
rm -f "$APPDIR"/usr/lib/libxkbcommon.so*

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
