#!/bin/bash
# Cross-build, as static PIC archives for aarch64-linux-android, every C library the Android
# firmware tools (libvphone_cli.so) link, from the pinned upstream releases in pins.txt.
# zlib is the NDK's. libusb's header comes from the pinned release; its implementation is the
# app's libusb-1.0-compatible virtual USB library, so libusb itself is not built here.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$here/../.." && pwd)
. "$ROOT/android/env.sh"
BUILD=$ROOT/android/build/create
SRC=$BUILD/deps-src
# aarch64 is the product. x86_64 only exists to run the same code natively on the emulator.
ARCH=${VPHONE_CREATE_ARCH:-aarch64}
if [ "$ARCH" = aarch64 ]; then WORK=$BUILD/deps-build PREFIX=$ROOT/android/build/out/create-deps
else WORK=$BUILD/$ARCH/deps-build PREFIX=$BUILD/$ARCH/deps; fi
JOBS=${JOBS:-$(nproc)}
mkdir -p "$SRC" "$WORK" "$PREFIX/lib/pkgconfig" "$PREFIX/include"
export TMPDIR=$BUILD/tmp; mkdir -p "$TMPDIR"
export PATH=$NDK_BIN:$PATH
export CC=$NDK_BIN/$ARCH-linux-android$ANDROID_API-clang CXX=$NDK_BIN/$ARCH-linux-android$ANDROID_API-clang++ AR=$NDK_BIN/llvm-ar RANLIB=$NDK_BIN/llvm-ranlib STRIP=$NDK_BIN/llvm-strip NM=$NDK_BIN/llvm-nm
export CFLAGS="-O2 -fPIC -g0 -D_FILE_OFFSET_BITS=64" CXXFLAGS="-O2 -fPIC -g0" CPPFLAGS="-I$PREFIX/include" LDFLAGS="-L$PREFIX/lib"
export PKG_CONFIG_LIBDIR=$PREFIX/lib/pkgconfig; unset PKG_CONFIG_PATH
HOST=$ARCH-linux-android
# bionic has pthreads in libc and no libpthread: the empty archive satisfies -lpthread in configure and Makefiles.
[ -f "$PREFIX/lib/libpthread.a" ] || "$AR" rcs "$PREFIX/lib/libpthread.a"

pin() { awk -F'\t' -v n="$1" '$1==n{print $'"$2"'}' "$here/pins.txt"; }
fetch() { # name -> prints archive path
  local url sha file; url=$(pin "$1" 2); sha=$(pin "$1" 3); file=$SRC/${url##*/}
  [ -f "$file" ] || { curl -fsSL --retry 3 -m 900 -o "$file.part" "$url"; mv "$file.part" "$file"; }
  echo "$sha  $file" | sha256sum -c --quiet - >&2
  echo "$file"
}
unpack() { # name -> prints source dir
  local a d; a=$(fetch "$1"); d=$WORK/$1; rm -rf "$d"; mkdir -p "$d"; tar -xf "$a" -C "$d" --strip-components=1; echo "$d"
}
stamped() { [ -f "$WORK/$1.stamp" ]; }
stamp() { touch "$WORK/$1.stamp"; echo "== $1 done"; }
autotools() { # name [configure args]
  local n=$1; shift; stamped "$n" && return 0
  local d; d=$(unpack "$n")
  ( cd "$d" && ./configure --host=$HOST --build=x86_64-pc-linux-gnu --prefix="$PREFIX" --enable-static --disable-shared "$@" >configure.out 2>&1 || { tail -n 40 configure.out; tail -n 40 config.log; exit 1; }
    make -j"$JOBS" >make.out 2>&1 || { tail -n 40 make.out; exit 1; }
    make install >/dev/null )
  stamp "$n"
}

if ! stamped xz; then autotools xz --disable-xz --disable-xzdec --disable-lzmadec --disable-lzmainfo --disable-scripts --disable-doc --disable-nls; fi

if ! stamped lzfse; then
  d=$(unpack lzfse); objs=()
  for c in lzfse_decode lzfse_decode_base lzfse_encode lzfse_encode_base lzfse_fse lzvn_decode_base lzvn_encode_base; do
    "$CC" $CFLAGS -c "$d/src/$c.c" -o "$d/$c.o"; objs+=("$d/$c.o"); done
  "$AR" rcs "$PREFIX/lib/liblzfse.a" "${objs[@]}"; install -m644 "$d/src/lzfse.h" "$PREFIX/include/lzfse.h"; stamp lzfse
fi

if ! stamped openssl; then
  d=$(unpack openssl)
  ( cd "$d"; export ANDROID_NDK_ROOT; ./Configure $([ "$ARCH" = aarch64 ] && echo android-arm64 || echo android-$ARCH) -D__ANDROID_API__=$ANDROID_API no-shared no-tests no-apps no-docs no-dso no-engine no-legacy --prefix="$PREFIX" --libdir=lib >cfg.out 2>&1 || { tail -n 30 cfg.out; exit 1; }
    make -j"$JOBS" >make.out 2>&1 || { tail -n 40 make.out; exit 1; }
    make install_sw >/dev/null )
  stamp openssl
fi

if ! stamped curl; then
  # No built-in bundle path exists on Android: --with-ca-fallback makes curl use OpenSSL's default
  # verify paths, which the Android tools point at the system store through SSL_CERT_FILE.
  autotools curl --with-openssl="$PREFIX" --with-zlib --without-libpsl --without-brotli --without-zstd --without-libidn2 \
    --without-nghttp2 --disable-ldap --disable-ldaps --disable-rtsp --disable-dict --disable-telnet --disable-tftp \
    --disable-pop3 --disable-imap --disable-smb --disable-smtp --disable-gopher --disable-mqtt --disable-manual \
    --disable-docs --without-ca-bundle --without-ca-path --with-ca-fallback
fi

export LIBS=""
if ! stamped libplist; then autotools libplist --without-cython --without-tools; fi
autotools libimobiledevice-glue
autotools libusbmuxd --without-tools --without-preflight 
autotools libtatsu
autotools libimobiledevice --without-cython --with-openssl

if ! stamped libarchive; then
  # archive.h includes contrib/android's android_lf.h on bionic; installed below with the headers.
  CPPFLAGS="$CPPFLAGS -I$WORK/libarchive/contrib/android/include" autotools libarchive --disable-bsdtar --disable-bsdcpio --disable-bsdcat --disable-bsdunzip --disable-acl \
    --without-xml2 --without-expat --without-bz2lib --without-lz4 --without-zstd --without-cng --without-nettle \
    --without-openssl --with-lzma --with-zlib
fi

[ -f "$WORK/libarchive/contrib/android/include/android_lf.h" ] && install -m644 "$WORK/libarchive/contrib/android/include/android_lf.h" "$PREFIX/include/android_lf.h"

# Emulator-only x86_64 verification build links the real upstream libusb; the aarch64 product links the vusb one.
if [ "$ARCH" != aarch64 ] && ! stamped libusb; then
  d=$(unpack libusb)
  ( cd "$d" && ./configure --host=$HOST --build=x86_64-pc-linux-gnu --prefix="$PREFIX" --disable-static --enable-shared --disable-udev --disable-examples-build >c.out 2>&1 && make -j"$JOBS" >m.out 2>&1 && make install >/dev/null )
  ( cd "$PREFIX/lib" && "$NDK_BIN/llvm-objcopy" --version >/dev/null && ln -sfn libusb-1.0.so libusb-1.0.so.0 )
  stamp libusb
fi
# libusb.h for compiling against the libusb-1.0 ABI.
if [ ! -f "$PREFIX/include/libusb-1.0/libusb.h" ]; then
  d=$(unpack libusb); mkdir -p "$PREFIX/include/libusb-1.0"; install -m644 "$d/libusb/libusb.h" "$PREFIX/include/libusb-1.0/libusb.h"
fi
echo "deps installed in $PREFIX"
ls "$PREFIX/lib"
