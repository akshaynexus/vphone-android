#!/bin/bash
# Build libvphone_apfs.so: the APFS tool for on-device VM creation (no root, no mount).
#
#   android/scripts/build-apfs.sh [--host]
#     -> android/build/out/libvphone_apfs.so   (aarch64-linux-android, PIE, 16 KiB alignment)
#     --host also builds android/build/apfs-target/release/vphone_apfs for this machine.
#
# The write engine is enesilhaydin/apfs-explorer (GPL-3.0-only), pinned below and carried with the
# patches in android/create/apfs/patches/apfs-explorer/, because it needs three changes (see the
# patch). It is fetched under android/build/, which git ignores.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$PWD
. android/env.sh

APFS_EXPLORER_URL=https://github.com/enesilhaydin/apfs-explorer.git
APFS_EXPLORER_REV=7d6cd8006e9234f9d4aabe9d132cb89227ecd6d3
DEP=$ROOT/android/build/apfs-deps/apfs-explorer
STAMP=$DEP/.vphone-patched-$APFS_EXPLORER_REV
PATCHES=$ROOT/android/create/apfs/patches/apfs-explorer

if [ ! -e "$STAMP" ]; then
    rm -rf "$DEP"
    mkdir -p "$DEP"
    git -C "$DEP" init -q
    git -C "$DEP" fetch -q --depth 1 "$APFS_EXPLORER_URL" "$APFS_EXPLORER_REV"
    git -C "$DEP" checkout -q FETCH_HEAD
    for p in "$PATCHES"/*.patch; do
        git -C "$DEP" apply "$p"
    done
    : >"$STAMP"
fi

export CARGO_TARGET_DIR=$ROOT/android/build/apfs-target
cd android/create/apfs
RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384" \
    cargo build --release --locked --target aarch64-linux-android --bin vphone_apfs
if [ "${1:-}" = --host ]; then
    cargo build --release --locked --bin vphone_apfs
fi
mkdir -p "$ROOT/android/build/out"
cp "$CARGO_TARGET_DIR/aarch64-linux-android/release/vphone_apfs" "$ROOT/android/build/out/libvphone_apfs.so"

READELF=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf
[ "$($READELF -h "$ROOT/android/build/out/libvphone_apfs.so" | awk '/Type:/{print $2}')" = DYN ] ||
    { echo "not a position-independent executable" >&2; exit 1; }
"$READELF" -lW "$ROOT/android/build/out/libvphone_apfs.so" | awk '/LOAD/{ if ($NF != "0x4000") bad=1 } END{ exit bad }' ||
    { echo "a LOAD segment is not aligned to 16 KiB" >&2; exit 1; }
echo "built android/build/out/libvphone_apfs.so"
