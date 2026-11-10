#!/usr/bin/env bash
# Host test of how the Turnip build reads a gralloc buffer's layout: compiles the u_gralloc
# sources of android/build/mesa (as android/scripts/build-turnip.sh patched them) with
# android/mesa/tests/gralloc_test.c and runs it. No device, nothing fetched.
set -euo pipefail

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
SRC=$REPO/android/build/mesa
OUT=$REPO/android/build/tmp/gralloc-test
G=$SRC/src/util/u_gralloc
[ -f "$G/u_gralloc_aimapper.c" ] || { echo "run android/scripts/build-turnip.sh first: $SRC is not patched" >&2; exit 1; }
mkdir -p "$OUT"
${CC:-cc} -std=gnu11 -O1 -g -Wall -Wno-unused-function -DHAS_FREEDRENO -DHAVE_PTHREAD -D_GNU_SOURCE \
    -DUTIL_ARCH_LITTLE_ENDIAN=1 -DUTIL_ARCH_BIG_ENDIAN=0 -DHAVE_STRUCT_TIMESPEC -DHAVE_ENDIAN_H \
    -I"$SRC/include/android_stub" -I"$SRC/include" -I"$SRC/src" -I"$G" \
    -o "$OUT/gralloc_test" \
    "$REPO/android/mesa/tests/gralloc_test.c" \
    "$G/u_gralloc_fallback.c" "$G/u_gralloc_internal.c" "$G/u_gralloc_qcom_native_handle.c" \
    "$REPO/android/mesa/tests/gralloc_test_stubs.c"
"$OUT/gralloc_test"
