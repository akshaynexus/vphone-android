#!/bin/bash
# Assembles android/build/out/vphone-android-debug.apk: libadrenotools (pinned, built here), the
# Rust library, and the libraries the QEMU and Mesa builds left in android/build/out.
set -euo pipefail

ANDROID_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
BUILD=$ANDROID_DIR/build
OUT=$BUILD/out
ADRENOTOOLS_URL=https://github.com/bylaws/libadrenotools.git
ADRENOTOOLS_REV=8fae8ce254dfc1344527e05301e43f37dea2df80

# shellcheck disable=SC1091
. "$ANDROID_DIR/env.sh"
export ANDROID_HOME=$ANDROID_SDK_ROOT   # Gradle refuses two different SDK paths
export TMPDIR=$BUILD/tmp
mkdir -p "$TMPDIR"
export GRADLE_OPTS="-Djava.io.tmpdir=$TMPDIR ${GRADLE_OPTS:-}"

QEMU_LIB=$OUT/lib/libqemu-vphone.so
[ -f "$QEMU_LIB" ] || QEMU_LIB=$OUT/libqemu-vphone.so
TURNIP_LIB=$OUT/libvulkan_freedreno.so
# On-device VM creation: PIE helpers run as lib*.so from nativeLibraryDir, and vphone-cli's Swift runtime.
CREATE=$OUT/create
CREATE_HELPERS=(
  "$CREATE/libvphone_cli.so"
  "$OUT/libusb-1.0.so" "$OUT/libvusbd.so" "$OUT/libusbmuxd_vphone.so" "$OUT/libvphone_portfwd.so"
  "$OUT/libvphone_apfs.so"
)
READELF=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-readelf

missing=()
for lib in "$QEMU_LIB" "$TURNIP_LIB" "${CREATE_HELPERS[@]}"; do
  [ -f "$lib" ] || missing+=("$lib")
done
if [ ${#missing[@]} -gt 0 ]; then
  echo "build-apk.sh: cannot package without:" >&2
  printf '  %s\n' "${missing[@]}" >&2
  exit 1
fi
for symbol in qemu_android_main rvg_android_set_vulkan_gipa rvg_android_set_window rvg_android_pointer; do
  if ! "$READELF" -W --dyn-syms "$QEMU_LIB" | grep -qw "$symbol"; then
    echo "build-apk.sh: $QEMU_LIB does not export $symbol" >&2
    exit 1
  fi
done

CMAKE=$(ls -d "$ANDROID_SDK_ROOT"/cmake/*/bin/cmake | sort -V | tail -1)
NINJA=$(dirname "$CMAKE")/ninja
TOOLS_SRC=$BUILD/src-adrenotools
TOOLS_BUILD=$BUILD/adrenotools
if [ "$(git -C "$TOOLS_SRC" rev-parse HEAD 2>/dev/null)" != "$ADRENOTOOLS_REV" ]; then
  rm -rf "$TOOLS_SRC"
  git clone -q "$ADRENOTOOLS_URL" "$TOOLS_SRC"
  git -C "$TOOLS_SRC" -c advice.detachedHead=false checkout -q "$ADRENOTOOLS_REV"
fi
git -C "$TOOLS_SRC" submodule update --init --recursive -q
"$CMAKE" -S "$TOOLS_SRC" -B "$TOOLS_BUILD" -G Ninja -DCMAKE_MAKE_PROGRAM="$NINJA" \
  -DCMAKE_TOOLCHAIN_FILE="$ANDROID_NDK_ROOT/build/cmake/android.toolchain.cmake" \
  -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM=android-35 -DANDROID_STL=c++_static \
  -DANDROID_SUPPORT_FLEXIBLE_PAGE_SIZES=ON -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON >/dev/null
"$CMAKE" --build "$TOOLS_BUILD" --target adrenotools main_hook hook_impl

export CARGO_TARGET_DIR=$BUILD/vphone-native-target
export RUSTFLAGS="-C link-arg=-Wl,-z,max-page-size=16384 ${RUSTFLAGS:-}"
cargo build --manifest-path "$ANDROID_DIR/vphone-native/Cargo.toml" --release --target aarch64-linux-android

STAGE=$BUILD/jniLibs/arm64-v8a
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$CARGO_TARGET_DIR/aarch64-linux-android/release/libvphone_native.so" "$STAGE/"
cargo build --manifest-path "$ANDROID_DIR/vphone-vm/Cargo.toml" --release --target aarch64-linux-android
cp "$CARGO_TARGET_DIR/aarch64-linux-android/release/vphone_vm" "$STAGE/libvphone_vm.so"
cp "$QEMU_LIB" "$TURNIP_LIB" "${CREATE_HELPERS[@]}" "$STAGE/"
# vphone-cli's Swift runtime and everything else its build left beside it.
for so in "$CREATE"/*.so; do
  [ -f "$STAGE/$(basename "$so")" ] || cp "$so" "$STAGE/"
done
find "$TOOLS_BUILD" -name 'lib*.so' \( -name libadrenotools.so -o -name libmain_hook.so -o -name libhook_impl.so \) -exec cp {} "$STAGE/" \;

# Every library a packaged one needs is packaged too, or is the platform's.
SYSROOT_LIBS=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/35
# The driver loads in the vendor namespace libadrenotools gives it, where these exist.
DRIVER_ONLY=" libhardware.so libsync.so libnativewindow.so "
for so in "$STAGE"/*.so; do
  for need in $("$READELF" -d "$so" | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p'); do
    [ -f "$STAGE/$need" ] && continue
    [ -f "$SYSROOT_LIBS/$need" ] && continue
    case "$(basename "$so"):$DRIVER_ONLY" in libvulkan_freedreno.so:*" $need "*) continue ;; esac
    dep=$(find "$OUT/deps" "$OUT/lib" "$OUT/create" -name "$need" -type f -print -quit)
    if [ -z "$dep" ]; then
      echo "build-apk.sh: $(basename "$so") needs $need, which nothing provides" >&2
      exit 1
    fi
    cp "$dep" "$STAGE/"
  done
done

# The helpers are exec'd: a helper that is not a PIE executable cannot be run from nativeLibraryDir.
for helper in libvphone_vm.so libvusbd.so libusbmuxd_vphone.so libvphone_portfwd.so libvphone_apfs.so libvphone_cli.so; do
  if ! "$READELF" -h "$STAGE/$helper" | grep -q 'Type:.*DYN'; then
    echo "build-apk.sh: $helper is not a PIE executable" >&2
    exit 1
  fi
done

cd "$ANDROID_DIR"
./gradlew --no-daemon --project-cache-dir "$BUILD/.gradle" :app:assembleDebug
cp "$BUILD/gradle/app/outputs/apk/debug/app-debug.apk" "$OUT/vphone-android-debug.apk"
echo "built $OUT/vphone-android-debug.apk"
