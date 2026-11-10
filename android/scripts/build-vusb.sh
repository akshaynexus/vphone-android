#!/bin/bash
# The on-device restore's USB transport: android/create/vusb (Rust) and stock usbmuxd with a 32 KiB MTU,
# built for aarch64-linux-android and for this host (the host build drives a real restore in
# android/create/vusb/verify-restore.sh).
#
#   android/scripts/build-vusb.sh
#
# Output, Android (16 KiB-aligned, PIE executables named lib*.so):
#   android/build/out/libusb-1.0.so, libvusbd.so, libusbmuxd_vphone.so, libvphone_portfwd.so
#   android/build/out/include/libusb-1.0/libusb.h (upstream header, for whatever links this libusb)
# Output, host: android/build/vusb/host/{libusb-1.0.so.0,vusbd,usbmuxd,vphone_portfwd}
#
# Every source is pinned by URL and SHA-256, or (crates) by Cargo.lock.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/android/env.sh"

VUSB=$ROOT/android/create/vusb
BUILD=$ROOT/android/build/vusb
OUT=$ROOT/android/build/out
SRC=$BUILD/src
HOST=$BUILD/host
PREFIX=$BUILD/android-deps
JOBS=${JOBS:-$(nproc)}
export CARGO_TARGET_DIR=$BUILD/target
mkdir -p "$SRC" "$HOST" "$PREFIX/lib/pkgconfig" "$PREFIX/include" "$OUT/include/libusb-1.0"

NDK_BIN=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin
CC=$NDK_BIN/aarch64-linux-android$ANDROID_API-clang
AR=$NDK_BIN/llvm-ar
RANLIB=$NDK_BIN/llvm-ranlib
STRIP=$NDK_BIN/llvm-strip
PAGE_FLAGS="-Wl,-z,max-page-size=16384 -Wl,-z,common-page-size=16384"

fetch() { # url sha256 file
  local url=$1 sha=$2 file=$SRC/$3
  if [ ! -f "$file" ]; then
    curl -fsSL --retry 3 -m 900 -o "$file.part" "$url"
    mv "$file.part" "$file"
  fi
  echo "$sha  $file" | sha256sum -c --quiet -
}

fetch https://github.com/libusb/libusb/releases/download/v1.0.27/libusb-1.0.27.tar.bz2 \
      ffaa41d741a8a3bee244ac8e54a72ea05bf2879663c098c82fc5757853441575 libusb-1.0.27.tar.bz2
fetch https://github.com/libimobiledevice/libplist/releases/download/2.7.0/libplist-2.7.0.tar.bz2 \
      7ac42301e896b1ebe3c654634780c82baa7cb70df8554e683ff89f7c2643eb8b libplist-2.7.0.tar.bz2
fetch https://github.com/libimobiledevice/libimobiledevice-glue/releases/download/1.3.2/libimobiledevice-glue-1.3.2.tar.bz2 \
      6489a3411b874ecd81c87815d863603f518b264a976319725e0ed59935546774 libimobiledevice-glue-1.3.2.tar.bz2
# The commit tools/build-usbmuxd-32k.sh pins.
USBMUXD_COMMIT=3ded00c9985a5108cfc7591a309f9a23d57a8cba
fetch https://github.com/libimobiledevice/usbmuxd/archive/$USBMUXD_COMMIT.tar.gz \
      fe4d04223b85add8bef6a2dd6452ef6f9bb1c0eb52ba036564c9aa8d1dd9dee2 usbmuxd-$USBMUXD_COMMIT.tar.gz

# libusb.h: only the public header is used, this tree implements the library.
HDR=$BUILD/libusb-header
if [ ! -f "$HDR/libusb.h" ]; then
  mkdir -p "$HDR"
  tar -xjf "$SRC/libusb-1.0.27.tar.bz2" -C "$HDR" --strip-components=2 libusb-1.0.27/libusb/libusb.h
fi
cp "$HDR/libusb.h" "$OUT/include/libusb-1.0/libusb.h"

# --- Rust: the broker, the library and the forwarder ----------------------------------------------------
(cd "$VUSB" && cargo build --release --locked -p vusbd -p usb_1_0 -p vusb-portfwd)
cp "$CARGO_TARGET_DIR/release/libusb_1_0.so" "$HOST/libusb-1.0.so.0"
ln -sf libusb-1.0.so.0 "$HOST/libusb-1.0.so"
cp "$CARGO_TARGET_DIR/release/vusbd" "$CARGO_TARGET_DIR/release/vphone_portfwd" "$HOST/"

(cd "$VUSB" && VUSB_LIBUSB_SONAME=libusb-1.0.so \
  RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 -C link-arg=-Wl,-z,common-page-size=16384" \
  cargo build --release --locked --target aarch64-linux-android -p vusbd -p usb_1_0 -p vusb-portfwd)
A=$CARGO_TARGET_DIR/aarch64-linux-android/release
cp "$A/libusb_1_0.so" "$OUT/libusb-1.0.so"
cp "$A/vusbd" "$OUT/libvusbd.so"
cp "$A/vphone_portfwd" "$OUT/libvphone_portfwd.so"

# --- libplist and libimobiledevice-glue for Android, static --------------------------------------------
PATCHES=$ROOT/android/deps-create/patches
build_autotools() { # name tarball configure-args; PATCH=file applies it first, and a changed patch rebuilds
  local name=$1 tarball=$2; shift 2
  local stamp=$BUILD/$name.stamp
  [ -z "${PATCH:-}" ] || stamp=$BUILD/$name-$(sha256sum "$PATCH" | cut -c1-12).stamp
  [ -f "$stamp" ] && return 0
  local d=$BUILD/android-src/$name
  rm -rf "$d"; mkdir -p "$d"
  tar -xjf "$SRC/$tarball" -C "$d" --strip-components=1
  [ -z "${PATCH:-}" ] || patch -d "$d" -p1 -s < "$PATCH"
  (cd "$d" && CC=$CC AR=$AR RANLIB=$RANLIB STRIP=$STRIP \
     CFLAGS="-O2 -fPIC -g0" \
     PKG_CONFIG_LIBDIR=$PREFIX/lib/pkgconfig PKG_CONFIG_PATH= \
     ./configure --host=aarch64-linux-android --prefix="$PREFIX" --libdir="$PREFIX/lib" \
       --disable-shared --enable-static --with-pic "$@" && \
   make -j"$JOBS" && make install)
  touch "$stamp"
}
build_autotools libplist libplist-2.7.0.tar.bz2 --without-cython --without-tools
# glue's timed waits count on a clock that leaves out time the process was stopped (a paused
# creation stops usbmuxd with the VM); usbmuxd's own ticks use the same clock, below.
GLUE_PATCH=$PATCHES/libimobiledevice-glue-active-clock.patch
PATCH=$GLUE_PATCH build_autotools glue libimobiledevice-glue-1.3.2.tar.bz2

# The same glue for this host, beside the host usbmuxd: the verification restore's tools, which
# find it through LD_LIBRARY_PATH, then keep time as the phone's do.
HOST_GLUE=$BUILD/host-glue
host_glue_stamp=$BUILD/host-glue-$(sha256sum "$GLUE_PATCH" | cut -c1-12).stamp
if [ ! -f "$host_glue_stamp" ]; then
  rm -rf "$HOST_GLUE"; mkdir -p "$HOST_GLUE/src"
  tar -xjf "$SRC/libimobiledevice-glue-1.3.2.tar.bz2" -C "$HOST_GLUE/src" --strip-components=1
  patch -d "$HOST_GLUE/src" -p1 -s < "$GLUE_PATCH"
  (cd "$HOST_GLUE/src" && ./configure --prefix="$HOST_GLUE" --libdir="$HOST_GLUE/lib" --disable-static >configure.out 2>&1 \
     && make -j"$JOBS" >make.out 2>&1 && make install >/dev/null) || { tail -n 30 "$HOST_GLUE/src/make.out" >&2; exit 1; }
  touch "$host_glue_stamp"
fi
cp "$HOST_GLUE/lib/libimobiledevice-glue-1.0.so.0" "$HOST/libimobiledevice-glue-1.0.so.0"

# --- usbmuxd, 32 KiB MTU, against this libusb (the steps in tools/build-usbmuxd-32k.sh) ------------------
USBMUXD=$BUILD/usbmuxd-src
if [ ! -f "$USBMUXD/src/usb.c" ]; then
  rm -rf "$USBMUXD"; mkdir -p "$USBMUXD"
  tar -xzf "$SRC/usbmuxd-$USBMUXD_COMMIT.tar.gz" -C "$USBMUXD" --strip-components=1
fi
sed -i 's/#define USB_MTU (3 \* 16384)/#define USB_MTU (2 * 16384)/' "$USBMUXD/src/usb.h"
grep -q '#define USB_MTU (2 \* 16384)' "$USBMUXD/src/usb.h"
grep -q vphone_active_ms "$USBMUXD/src/utils.c" || patch -d "$USBMUXD" -p1 -s < "$PATCHES/usbmuxd-active-clock.patch"
grep -q vphone_active_ms "$USBMUXD/src/utils.c"
USBMUXD_SRCS=(src/client.c src/device.c src/preflight.c src/log.c src/usb.c src/utils.c src/conf.c src/main.c)
DEFINES=(-DPACKAGE_NAME='"usbmuxd"' -DPACKAGE_VERSION='"1.1.1-32k"' -DPACKAGE_STRING='"usbmuxd 1.1.1-32k"'
  -DPACKAGE_URL='"https://libimobiledevice.org"' -DPACKAGE_BUGREPORT='"https://github.com/libimobiledevice/usbmuxd/issues"'
  -DHAVE_CLOCK_GETTIME=1 -DHAVE_LOCALTIME_R=1 -DHAVE_PPOLL=1 -DHAVE_SYS_TYPES_H=1 -DHAVE_LIBUSB_HOTPLUG_API=1)

# Host, against this library, for the verification restore.
(cd "$USBMUXD" && gcc -O2 -Wall -Isrc -I"$OUT/include/libusb-1.0" -I"$HOST_GLUE/include" $(pkg-config --cflags libplist-2.0) \
  "${DEFINES[@]}" "${USBMUXD_SRCS[@]}" -o "$HOST/usbmuxd" \
  -L"$HOST" -l:libusb-1.0.so.0 -l:libimobiledevice-glue-1.0.so.0 $(pkg-config --libs libplist-2.0) -lpthread -lm)

# Android: libplist and glue linked statically, libusb-1.0.so dynamically.
(cd "$USBMUXD" && "$CC" -O2 -fPIE -Isrc -I"$OUT/include/libusb-1.0" -I"$PREFIX/include" \
  "${DEFINES[@]}" "${USBMUXD_SRCS[@]}" -o "$OUT/libusbmuxd_vphone.so" -pie $PAGE_FLAGS \
  -L"$OUT" -lusb-1.0 "$PREFIX/lib/libimobiledevice-glue-1.0.a" "$PREFIX/lib/libplist-2.0.a" -lm -latomic)
"$STRIP" "$OUT/libusb-1.0.so" "$OUT/libvusbd.so" "$OUT/libusbmuxd_vphone.so" "$OUT/libvphone_portfwd.so"

echo "== Android output"
for f in libusb-1.0.so libvusbd.so libusbmuxd_vphone.so libvphone_portfwd.so; do
  align=$("$NDK_BIN/llvm-readelf" -lW "$OUT/$f" | awk '$1=="LOAD" && !x{print $NF; x=1}')
  [ "$align" = 0x4000 ] || { echo "$f: LOAD alignment $align, expected 0x4000" >&2; exit 1; }
  type=$("$NDK_BIN/llvm-readelf" -h "$OUT/$f" | awk '$1=="Type:"{print $2}')
  echo "$f type=$type align=$align needed: $("$NDK_BIN/llvm-readelf" -d "$OUT/$f" | sed -n 's/.*(NEEDED).*\[\(.*\)\].*/\1/p' | tr '\n' ' ')"
done
