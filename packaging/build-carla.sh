#!/usr/bin/env bash
# Builds the Carla that Carlita is built against and bundles, and prints the
# prefix it is installed in (with lib/carla, include/carla, share/carla).
#
# Carla's last release (2.5.x) has no CLAP support and reads VST3 plugins
# through JUCE, which fails on several native Linux VST3 plugins (u-he,
# sfizz, MT-PowerDrumKit...) that load fine elsewhere. Carla's main branch
# has CLAP and its own VST3 host, so a pinned commit of it is built, with
# the patches in appimage/carla-patches applied.
#
#   CARLA_REF        commit to build (default: the pinned one below)
#   CARLA_BUILD_DIR  where to build and install (default: target/carla)
#
# A finished build is reused while the commit and patches are unchanged.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REF="${CARLA_REF:-97a9e0740baf6df2df942495c02532a624c44682}"
DIR="${CARLA_BUILD_DIR:-$ROOT/target/carla}"
PATCHES="$ROOT/packaging/appimage/carla-patches"
SRC="$DIR/src"
PREFIX="$DIR/install/usr"

STAMP="$REF $(cat "$PATCHES"/*.patch | sha256sum | cut -d' ' -f1)"
if [ -x "$PREFIX/lib/carla/carla-discovery-native" ] \
    && [ "$(cat "$DIR/stamp" 2>/dev/null)" = "$STAMP" ]; then
    echo "$PREFIX"
    exit 0
fi

{
    rm -rf "$SRC" "$DIR/install" "$DIR/stamp"
    mkdir -p "$SRC"
    git -C "$SRC" init --quiet
    git -C "$SRC" fetch --quiet --depth 1 https://github.com/falkTX/Carla.git "$REF"
    git -C "$SRC" checkout --quiet FETCH_HEAD
    for patch in "$PATCHES"/*.patch; do
        git -C "$SRC" apply "$patch"
    done
    make -C "$SRC" -j"$(nproc)"
    make -C "$SRC" install PREFIX=/usr DESTDIR="$DIR/install"
    echo "$STAMP" >"$DIR/stamp"
} >&2

echo "$PREFIX"
