#!/bin/bash
# Cross-build QEMU's dependencies for aarch64-linux-android as static PIC
# archives, from pinned upstream source releases, into android/build/out/deps.
# Each dependency is verified against its SHA-256 and rebuilt only if its stamp
# is missing. zlib comes from the NDK (libz.so is a public NDK library).
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/android/env.sh"

BUILD=$ROOT/android/build
SRC=$BUILD/deps-src
WORK=$BUILD/deps-build
PREFIX=$BUILD/out/deps
JOBS=${JOBS:-$(nproc)}
mkdir -p "$SRC" "$WORK" "$PREFIX/lib/pkgconfig" "$PREFIX/include"

CC=$CC_aarch64_linux_android
CXX=$CXX_aarch64_linux_android
AR=$NDK_BIN/llvm-ar
RANLIB=$NDK_BIN/llvm-ranlib
STRIP=$NDK_BIN/llvm-strip
NM=$NDK_BIN/llvm-nm
HOST=aarch64-linux-android
CFLAGS="-O2 -fPIC -g0 -D_FILE_OFFSET_BITS=64"
export PKG_CONFIG_LIBDIR=$PREFIX/lib/pkgconfig
unset PKG_CONFIG_PATH

CROSS=$WORK/android-cross.ini
cat > "$CROSS" <<CROSSEOF
[binaries]
c = '$CC'
cpp = '$CXX'
ar = '$AR'
ranlib = '$RANLIB'
strip = '$STRIP'
nm = '$NM'
pkg-config = 'pkg-config'

[host_machine]
system = 'android'
cpu_family = 'aarch64'
cpu = 'armv8-a'
endian = 'little'

[built-in options]
c_args = ['-O2', '-fPIC', '-g0', '-D_FILE_OFFSET_BITS=64']
cpp_args = ['-O2', '-fPIC', '-g0']
c_link_args = ['-L$PREFIX/lib']
cpp_link_args = ['-L$PREFIX/lib']

[properties]
pkg_config_libdir = '$PREFIX/lib/pkgconfig'
growing_stack = false
have_c99_vsnprintf = true
have_c99_snprintf = true
have_unix98_printf = true
have_strlcpy = true
CROSSEOF

fetch() { # url sha256 file
  local url=$1 sha=$2 file=$SRC/${1##*/}
  if [ ! -f "$file" ]; then
    curl -fsSL --retry 3 -m 600 -o "$file.part" "$url"
    mv "$file.part" "$file"
  fi
  echo "$sha  $file" | sha256sum -c --quiet -
}

unpack() { # file dirname -> prints source dir
  local dir=$WORK/$2
  rm -rf "$dir"
  mkdir -p "$dir"
  tar -xf "$SRC/$1" -C "$dir" --strip-components=1
  echo "$dir"
}

stamp() { [ -f "$WORK/$1.stamp" ]; }
done_stamp() { touch "$WORK/$1.stamp"; echo "== $1: done"; }

autotools() { # srcdir [configure args...]
  local src=$1; shift
  (cd "$src" && CC=$CC CXX=$CXX AR=$AR RANLIB=$RANLIB NM=$NM STRIP=$STRIP \
     CFLAGS="$CFLAGS" CXXFLAGS="$CFLAGS" CPPFLAGS="-I$PREFIX/include" \
     LDFLAGS="-L$PREFIX/lib" \
     ./configure --host=$HOST --prefix="$PREFIX" --libdir="$PREFIX/lib" \
       --disable-shared --enable-static --with-pic "$@" && \
   make -j"$JOBS" && make install)
}

meson_static() { # srcdir [meson args...]
  local src=$1; shift
  meson setup "$src/_build" "$src" --cross-file "$CROSS" --prefix "$PREFIX" \
    --libdir lib --buildtype release --default-library static \
    --wrap-mode default "$@"
  ninja -C "$src/_build" -j"$JOBS"
  ninja -C "$src/_build" install
}

# zlib from the NDK: only a .pc file so that pkg-config users find it.
if ! stamp zlib; then
  cat > "$PREFIX/lib/pkgconfig/zlib.pc" <<PCEOF
Name: zlib
Description: zlib from the Android NDK sysroot
Version: 1.3.1
Libs: -lz
PCEOF
  done_stamp zlib
fi

if ! stamp libffi; then
  fetch https://github.com/libffi/libffi/releases/download/v3.5.2/libffi-3.5.2.tar.gz \
        f3a3082a23b37c293a4fcd1053147b371f2ff91fa7ea1b2a52e335676bac82dc
  d=$(unpack libffi-3.5.2.tar.gz libffi)
  autotools "$d" --disable-docs --disable-multi-os-directory --disable-exec-static-tramp
  done_stamp libffi
fi

if ! stamp pcre2; then
  fetch https://github.com/PCRE2Project/pcre2/releases/download/pcre2-10.45/pcre2-10.45.tar.bz2 \
        21547f3516120c75597e5b30a992e27a592a31950b5140e7b8bfde3f192033c4
  d=$(unpack pcre2-10.45.tar.bz2 pcre2)
  autotools "$d" --enable-pcre2-8 --disable-pcre2-16 --disable-pcre2-32 --disable-pcre2grep-libz \
    --disable-pcre2grep-libbz2 --enable-jit=auto
  done_stamp pcre2
fi

if ! stamp glib; then
  fetch https://download.gnome.org/sources/glib/2.84/glib-2.84.4.tar.xz \
        8a9ea10943c36fc117e253f80c91e477b673525ae45762942858aef57631bb90
  d=$(unpack glib-2.84.4.tar.xz glib)
  meson_static "$d" -Dtests=false -Dinstalled_tests=false -Dnls=disabled -Dintrospection=disabled \
    -Dlibmount=disabled -Dselinux=disabled -Dxattr=false -Dman-pages=disabled -Ddtrace=disabled \
    -Dsystemtap=disabled -Dsysprof=disabled -Ddocumentation=false -Dgio_module_dir=lib/gio/modules \
    -Dbsymbolic_functions=false -Dglib_debug=disabled -Doss_fuzz=disabled -Dlibelf=disabled
  done_stamp glib
fi

if ! stamp pixman; then
  fetch https://www.cairographics.org/releases/pixman-0.46.4.tar.gz \
        d09c44ebc3bd5bee7021c79f922fe8fb2fb57f7320f55e97ff9914d2346a591c
  d=$(unpack pixman-0.46.4.tar.gz pixman)
  meson_static "$d" -Dtests=disabled -Ddemos=disabled -Dgtk=disabled -Dlibpng=disabled -Dopenmp=disabled
  done_stamp pixman
fi

if ! stamp zstd; then
  fetch https://github.com/facebook/zstd/releases/download/v1.5.7/zstd-1.5.7.tar.gz \
        eb33e51f49a15e023950cd7825ca74a4a2b43db8354825ac24fc1b7ee09e6fa3
  d=$(unpack zstd-1.5.7.tar.gz zstd)
  make -C "$d/lib" -j"$JOBS" CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="$CFLAGS" PREFIX="$PREFIX" \
    ZSTD_LEGACY_SUPPORT=0 ZSTD_LIB_DEPRECATED=0 libzstd.a-mt libzstd.pc
  make -C "$d/lib" CC="$CC" AR="$AR" RANLIB="$RANLIB" CFLAGS="$CFLAGS" PREFIX="$PREFIX" \
    install-static install-includes install-pc
  done_stamp zstd
fi

if ! stamp lzfse; then
  fetch https://github.com/lzfse/lzfse/archive/refs/tags/lzfse-1.0.tar.gz \
        cf85f373f09e9177c0b21dbfbb427efaedc02d035d2aade65eb58a3cbf9ad267
  d=$(unpack lzfse-1.0.tar.gz lzfse)
  objs=()
  for c in lzfse_decode lzfse_decode_base lzfse_encode lzfse_encode_base lzfse_fse \
           lzvn_decode_base lzvn_encode_base; do
    "$CC" $CFLAGS -DNDEBUG -Wno-implicit-function-declaration -c "$d/src/$c.c" -o "$d/$c.o"
    objs+=("$d/$c.o")
  done
  "$AR" rcs "$PREFIX/lib/liblzfse.a" "${objs[@]}"
  install -m644 "$d/src/lzfse.h" "$PREFIX/include/lzfse.h"
  done_stamp lzfse
fi

if ! stamp dtc; then
  fetch https://mirrors.edge.kernel.org/pub/software/utils/dtc/dtc-1.7.2.tar.xz \
        92d8ca769805ae1f176204230438fe52808f4e1c7944053c9eec0e649b237539
  d=$(unpack dtc-1.7.2.tar.xz dtc)
  meson_static "$d" -Dtools=false -Dyaml=disabled -Dpython=disabled -Dvalgrind=disabled -Dtests=false
  # dtc installs a shared libfdt even for a static build, and a linker prefers it.
  rm -f "$PREFIX"/lib/libfdt.so*
  done_stamp dtc
fi

if ! stamp gmp; then
  fetch https://ftp.gnu.org/gnu/gmp/gmp-6.3.0.tar.xz \
        a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898
  d=$(unpack gmp-6.3.0.tar.xz gmp)
  autotools "$d" --enable-cxx=no
  done_stamp gmp
fi

if ! stamp nettle; then
  fetch https://ftp.gnu.org/gnu/nettle/nettle-3.10.2.tar.gz \
        fe9ff51cb1f2abb5e65a6b8c10a92da0ab5ab6eaf26e7fc2b675c45f1fb519b5
  d=$(unpack nettle-3.10.2.tar.gz nettle)
  autotools "$d" --enable-fat --disable-documentation --disable-openssl
  done_stamp nettle
fi

if ! stamp libpng; then
  fetch https://download.sourceforge.net/libpng/libpng-1.6.50.tar.xz \
        4df396518620a7aa3651443e87d1b2862e4e88cad135a8b93423e01706232307
  d=$(unpack libpng-1.6.50.tar.xz libpng)
  autotools "$d" --disable-tools --disable-tests
  done_stamp libpng
fi

if ! stamp tasn1; then
  fetch https://ftp.gnu.org/gnu/libtasn1/libtasn1-4.20.0.tar.gz \
        92e0e3bd4c02d4aeee76036b2ddd83f0c732ba4cda5cb71d583272b23587a76c
  d=$(unpack libtasn1-4.20.0.tar.gz libtasn1)
  autotools "$d" --disable-doc --disable-gtk-doc
  done_stamp tasn1
fi

echo "== all dependencies in $PREFIX"
