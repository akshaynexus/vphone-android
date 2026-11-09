#!/bin/bash
# Build the Android firmware tools: pinned C dependencies, the composed vphone-cli tree, and
# android/build/out/create/ with libvphone_cli.so and every Swift runtime library it needs.
# Needs swiftly's Swift 6.4.1 (linux-create/.swift-version), NDK 28.2 (android/env.sh), and
# worker 2's android/build/out/libusb-1.0.so to link against.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
. "$ROOT/android/env.sh"
WORK=$ROOT/android/build/create
OUT=$ROOT/android/build/out/create
SWIFT=${VPHONE_SWIFT:-$HOME/.local/share/swiftly/toolchains/6.4.1/usr/bin/swift}
SDK_URL=https://download.swift.org/swift-6.4.1-release/android-sdk/swift-6.4.1-RELEASE/swift-6.4.1-RELEASE_android.artifactbundle.tar.gz
SDK_SHA=a356b79f8f1daa6ae3a66f4c556966ca20c0208b3659805cf30a83a0a31d6772
READELF=$NDK_BIN/llvm-readelf
mkdir -p "$WORK/dl" "$WORK/swift-sdks" "$WORK/tmp" "$OUT"
export TMPDIR=$WORK/tmp

[ -e "$ROOT/android/build/out/libusb-1.0.so" ] || { echo "android/build/out/libusb-1.0.so is missing: run android/scripts/build-vusb.sh" >&2; exit 1; }

"$ROOT/android/deps-create/build-deps.sh"

SDKDIR=$WORK/swift-sdks/swift-6.4.1-RELEASE_android.artifactbundle
if [ ! -d "$SDKDIR" ]; then
  [ -f "$WORK/dl/sdk.tar.gz" ] || curl -fsSL --retry 3 -o "$WORK/dl/sdk.tar.gz" "$SDK_URL"
  echo "$SDK_SHA  $WORK/dl/sdk.tar.gz" | sha256sum -c --quiet -
  "$SWIFT" sdk install "$WORK/dl/sdk.tar.gz" --swift-sdks-path "$WORK/swift-sdks"
fi
ANDROID_NDK_HOME=$ANDROID_NDK_ROOT bash "$SDKDIR/swift-android/scripts/setup-android-sdk.sh" >/dev/null

"$ROOT/android/create/swift/sync-android.sh"
VPHONE_SWIFT=$SWIFT "$ROOT/android/create/swift/build-swift.sh" "$@"

BIN=$WORK/swift-build/out/Products/Release-android-aarch64/vphone-cli
LIBS=$SDKDIR/swift-android/swift-resources/usr/lib/swift-aarch64/android
rm -rf "$OUT"; mkdir -p "$OUT"
"$NDK_BIN/llvm-strip" --strip-unneeded -o "$OUT/libvphone_cli.so" "$BIN"
# Android loads exactly what DT_NEEDED names; copy the closure, then refuse anything unaccounted for.
ANDROID_LIBS=' libc.so libm.so libdl.so libz.so liblog.so libandroid.so '
queue=("$OUT/libvphone_cli.so")
while [ ${#queue[@]} -gt 0 ]; do
  f=${queue[0]}; queue=("${queue[@]:1}")
  while read -r n; do
    case "$ANDROID_LIBS" in *" $n "*) continue;; esac
    [ -e "$OUT/$n" ] && continue
    if [ -e "$LIBS/$n" ]; then s=$LIBS/$n
    elif [ "$n" = libc++_shared.so ]; then s=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/aarch64-linux-android/libc++_shared.so
    elif [ "$n" = libusb-1.0.so ]; then continue # shipped by the vusb build
    else echo "unresolved DT_NEEDED $n in $f" >&2; exit 1; fi
    "$NDK_BIN/llvm-strip" --strip-unneeded -o "$OUT/$n" "$s"; queue+=("$OUT/$n")
  done < <("$READELF" -d "$f" | sed -n 's/.*Shared library: \[\(.*\)\]/\1/p')
done
bad=0
for f in "$OUT"/*.so; do
  a=$("$READELF" -lW "$f" | awk '$1=="LOAD"{print $NF}' | sort -u | tr '\n' ' ')
  case "$a" in *0x1000*) echo "NOT 16 KiB aligned: $f ($a)" >&2; bad=1;; esac
done
[ "$bad" = 0 ] || exit 1
ls -la "$OUT"
echo "android firmware tools in $OUT"
