#!/bin/bash
# Writes the Android changes in the qemu, reims-vgpu and metal2vulkan submodules to
# android/patches/<name>.patch, new files included. The submodules stay pinned to the upstream
# forks; these patches are the source of everything this repository changes in them.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
TMP=$ROOT/android/build/tmp
mkdir -p "$TMP" "$ROOT/android/patches"
for name in qemu reims-vgpu metal2vulkan; do
  index=$TMP/export-$name.index
  cp -f "$(git -C "$ROOT/$name" rev-parse --absolute-git-dir)/index" "$index"
  GIT_INDEX_FILE=$index git -C "$ROOT/$name" add -A
  GIT_INDEX_FILE=$index git -C "$ROOT/$name" diff --cached --binary --full-index HEAD \
    > "$ROOT/android/patches/$name.patch"
  rm -f "$index"
  echo "$name: $(wc -c < "$ROOT/android/patches/$name.patch") bytes"
done
