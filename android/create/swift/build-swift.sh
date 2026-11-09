#!/bin/bash
# swift build of the composed Android tree. Arguments go to swift build.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
WORK=$ROOT/android/build/create
ARCH=${VPHONE_CREATE_ARCH:-aarch64}
if [ "$ARCH" = aarch64 ]; then PREFIX=$ROOT/android/build/out/create-deps USBLIB=$ROOT/android/build/out SCRATCH=$WORK/swift-build
else PREFIX=$WORK/$ARCH/deps USBLIB=$WORK/$ARCH/deps/lib SCRATCH=$WORK/$ARCH/swift-build; fi
export TMPDIR=$WORK/tmp; mkdir -p "$TMPDIR"
S=${VPHONE_SWIFT:-$HOME/.local/share/swiftly/toolchains/6.4.1/usr/bin/swift}
cd "$WORK/tree"
"$S" build --swift-sdk $ARCH-unknown-linux-android35 --swift-sdks-path "$WORK/swift-sdks" \
  --disable-sandbox -c release --scratch-path "$SCRATCH" \
  -Xcc -I"$PREFIX/include" -Xcc -I"$PREFIX/include/libusb-1.0" -Xcc -D_GNU_SOURCE \
  -Xlinker -L"$PREFIX/lib" -Xlinker -L"$USBLIB" -Xlinker -z -Xlinker nostart-stop-gc \
  -Xlinker -z -Xlinker max-page-size=16384 -Xlinker --no-undefined-version \
  -Xswiftc -I"$PREFIX/include" "$@"
