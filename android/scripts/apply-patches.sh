#!/bin/bash
# Applies android/patches/<name>.patch to the pinned submodules. Safe to re-run.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
git -C "$ROOT" submodule update --init qemu reims-vgpu metal2vulkan
git -C "$ROOT/qemu" submodule update --init util/mlib
for name in qemu reims-vgpu metal2vulkan; do
  patch=$ROOT/android/patches/$name.patch
  if git -C "$ROOT/$name" apply --reverse --check "$patch" 2>/dev/null; then
    echo "$name: already applied"
  else
    git -C "$ROOT/$name" apply --binary "$patch"
    echo "$name: applied"
  fi
done
