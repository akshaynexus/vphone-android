#!/usr/bin/env bash
# Builds Turnip (Mesa's freedreno Vulkan driver) for Android bionic against KGSL, for unrooted
# Adreno devices through libadrenotools (the S24 Ultra's A750, the OnePlus 15's A840), and writes
# android/build/out/libvulkan_freedreno.so. See android/mesa/README.md for what is built and why.
#
#   android/scripts/build-turnip.sh           build (fetches Mesa on first use)
#   android/scripts/build-turnip.sh --check   only inspect the library already built
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck disable=SC1091
. "$REPO/android/env.sh"

MESA_URL=https://gitlab.freedesktop.org/mesa/mesa.git
# main of 2026-11-08: has both A840 configurations (0x44050A31, 0x44050A21), the KGSL path and
# the state the patches below are made against. android/mesa/README.md says why this is not a tag.
MESA_COMMIT=316009797d8de1efdda53646f527821d2993ce24
SRC=$REPO/android/build/mesa
PATCHES=$REPO/android/patches/mesa
BUILD=$REPO/android/build/turnip
OUT=$REPO/android/build/out
SO=$OUT/libvulkan_freedreno.so
NDK_BIN=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin

check() {
    local so=$1 needed bad exports strings_in
    "$NDK_BIN/llvm-readelf" -h "$so" | grep 'Machine:.*AArch64' >/dev/null \
        || { echo "not aarch64: $so" >&2; exit 1; }
    needed=$("$NDK_BIN/llvm-readelf" -d "$so" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
    echo "DT_NEEDED: $(echo "$needed" | tr '\n' ' ')"
    # What Android provides to a driver in the sphal namespace; anything else would not load.
    bad=$(echo "$needed" | grep -v -x -E 'lib(c|m|dl|log|hardware|nativewindow|sync|z)\.so' || true)
    [ -z "$bad" ] || { echo "DT_NEEDED outside what Android provides: $bad" >&2; exit 1; }
    # The system Vulkan loader finds a driver by its HAL module symbol, its only export.
    exports=$("$NDK_BIN/llvm-nm" -D --defined-only "$so")
    grep -E ' HMI$' <<<"$exports" >/dev/null || { echo "missing export HMI" >&2; exit 1; }
    echo "exports: $(awk '{print $3}' <<<"$exports" | tr '\n' ' ')"
    "$NDK_BIN/llvm-readelf" -lW "$so" \
        | awk '$1=="LOAD" && $NF!="0x4000" {print "LOAD not 16K aligned: " $0; bad=1} END{exit bad}'
    strings_in=$("$NDK_BIN/llvm-strings" -a "$so")
    for name in VK_EXT_external_memory_host VK_ANDROID_native_buffer vkGetMemoryHostPointerPropertiesEXT \
                VK_EXT_post_depth_coverage VK_EXT_fragment_shader_interlock; do
        grep -x "$name" <<<"$strings_in" >/dev/null || { echo "$name is absent from the library" >&2; exit 1; }
    done
    echo "ok: $so"
}

if [ "${1-}" = --check ]; then
    check "$SO"
    exit 0
fi

for tool in meson ninja git python3 glslangValidator bison flex; do
    command -v "$tool" >/dev/null || { echo "$tool is needed on the build host" >&2; exit 1; }
done
python3 -c 'import mako, yaml' || { echo "python3 needs mako and PyYAML" >&2; exit 1; }
[ -x "$CC_aarch64_linux_android" ] || { echo "no NDK at $ANDROID_NDK_ROOT" >&2; exit 1; }

# The tree is ours (android/build is ignored). It is rebuilt from the pin when the patches change,
# and left alone otherwise so a rebuild is incremental.
STAMP=$(cat "$PATCHES"/*.patch | sha256sum | cut -d' ' -f1)-$MESA_COMMIT
if [ ! -d "$SRC/.git" ]; then
    mkdir -p "$SRC"
    git -C "$SRC" init -q
    git -C "$SRC" remote add origin "$MESA_URL"
fi
if [ "$(cat "$SRC/.vphone-patched" 2>/dev/null || true)" != "$STAMP" ]; then
    git -C "$SRC" fetch --depth 1 origin "$MESA_COMMIT"
    git -C "$SRC" checkout -q --force --detach "$MESA_COMMIT"
    git -C "$SRC" clean -qfdx
    for patch in "$PATCHES"/*.patch; do
        echo "applying $(basename "$patch")"
        git -C "$SRC" apply --whitespace=nowarn "$patch"
    done
    echo "$STAMP" > "$SRC/.vphone-patched"
fi

mkdir -p "$BUILD" "$OUT"
CROSS=$BUILD/cross-android-aarch64.ini
# 16K page alignment (Android 15+ devices may run 16K kernels); the C++ runtime linked in, because
# libc++_shared.so is not on a stock device's library path for a driver.
LINK_ARGS="'-Wl,-z,max-page-size=16384', '-Wl,--no-undefined', '-static-libstdc++'"
cat > "$CROSS" <<CROSSINI
[binaries]
c = '$CC_aarch64_linux_android'
cpp = '$CXX_aarch64_linux_android'
ar = '$NDK_BIN/llvm-ar'
strip = '$NDK_BIN/llvm-strip'
pkg-config = '/bin/false'

[host_machine]
system = 'android'
cpu_family = 'aarch64'
cpu = 'aarch64'
endian = 'little'

[built-in options]
c_args = ['-DANDROID_API_LEVEL=$ANDROID_API']
cpp_args = ['-DANDROID_API_LEVEL=$ANDROID_API']
c_link_args = [$LINK_ARGS]
cpp_link_args = [$LINK_ARGS]
CROSSINI

OPTS=(
    --cross-file "$CROSS"
    -Dbuildtype=release -Db_ndebug=true
    -Dplatforms=android -Dandroid-stub=true -Dplatform-sdk-version="$ANDROID_API"
    -Dandroid-libbacktrace=disabled
    -Dgallium-drivers= -Dvulkan-drivers=freedreno -Dfreedreno-kmds=kgsl
    -Degl=disabled -Dgbm=disabled -Dglx=disabled -Dopengl=false -Dgles1=disabled -Dgles2=disabled
    -Dllvm=disabled -Dshared-llvm=disabled -Dlibunwind=disabled -Dzstd=disabled -Dvalgrind=disabled
    -Dvulkan-beta=false -Dvideo-codecs=
)
if [ -f "$BUILD/build.ninja" ]; then
    meson setup --reconfigure "$BUILD" "$SRC" "${OPTS[@]}"
else
    meson setup "$BUILD" "$SRC" "${OPTS[@]}"
fi
ninja -C "$BUILD" src/freedreno/vulkan/libvulkan_freedreno.so
"$NDK_BIN/llvm-strip" --strip-unneeded -o "$SO" "$BUILD/src/freedreno/vulkan/libvulkan_freedreno.so"
check "$SO"
