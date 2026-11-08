#!/bin/bash
# Build QEMU for an Android app: libqemu-vphone.so (aarch64-softmmu, TCG),
# which runs inside the app's process. Links reims-vgpu statically, from
# android/build/out/reims-dist (android/scripts/build-reims.sh), and the
# dependencies from android/build/out/deps (android/scripts/build-deps.sh).
#
# WITHOUT_REIMS=1 builds without the GPU device, into build/qemu-nopvg, for
# compiling QEMU before reims-vgpu exists.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/android/env.sh"

BUILD=$ROOT/android/build
DEPS=$BUILD/out/deps
REIMS=$BUILD/out/reims-dist
JOBS=${JOBS:-$(nproc)}

[ -f "$DEPS/lib/pkgconfig/glib-2.0.pc" ] || { echo "run android/scripts/build-deps.sh first" >&2; exit 1; }

pvg=--enable-pvg
QBUILD=$BUILD/qemu-release
PKG_LIBDIR=$DEPS/lib/pkgconfig
if [ "${WITHOUT_REIMS:-0}" = 1 ]; then
  pvg=--disable-pvg
  QBUILD=$BUILD/qemu-nopvg
else
  [ -f "$REIMS/lib/pkgconfig/reims-vgpu.pc" ] || { echo "run android/scripts/build-reims.sh first" >&2; exit 1; }
  PKG_LIBDIR=$REIMS/lib/pkgconfig:$PKG_LIBDIR
fi

export CC=$CC_aarch64_linux_android CXX=$CXX_aarch64_linux_android
export AR=$NDK_BIN/llvm-ar NM=$NDK_BIN/llvm-nm RANLIB=$NDK_BIN/llvm-ranlib
export STRIP=$NDK_BIN/llvm-strip OBJCOPY=$NDK_BIN/llvm-objcopy READELF=$NDK_BIN/llvm-readelf
export PKG_CONFIG=$ROOT/android/deps/pkg-config-static
export PKG_CONFIG_LIBDIR=$PKG_LIBDIR
unset PKG_CONFIG_PATH

if [ ! -f "$QBUILD/build.ninja" ]; then
  mkdir -p "$QBUILD"
  (cd "$QBUILD" && "$ROOT/qemu/configure" \
    --cross-prefix="$NDK_BIN/llvm-" --host-cc=cc --cpu=aarch64 \
    --target-list=aarch64-softmmu --prefix=/ \
    --without-default-features --disable-pie --disable-debug-info -Db_staticpic=true \
    --extra-cflags="-I$DEPS/include -g1 -D_FILE_OFFSET_BITS=64 -march=armv8.2-a+lse" \
    --extra-ldflags="-L$DEPS/lib" \
    --enable-tcg --disable-kvm --disable-werror --disable-qom-cast-debug \
    --disable-tools --disable-guest-agent \
    --enable-fdt=system --enable-pixman --enable-zstd --enable-lzfse \
    --enable-nettle --disable-gnutls --disable-gcrypt --enable-png \
    $pvg)
fi

ninja -C "$QBUILD" -j"$JOBS" libqemu-vphone.so

mkdir -p "$BUILD/out/lib" "$BUILD/out/symbols"
cp -f "$QBUILD/libqemu-vphone.so" "$BUILD/out/symbols/libqemu-vphone.so"
"$STRIP" --strip-unneeded -o "$BUILD/out/lib/libqemu-vphone.so" "$QBUILD/libqemu-vphone.so"
echo "== $BUILD/out/lib/libqemu-vphone.so (with debug info: out/symbols/)"
