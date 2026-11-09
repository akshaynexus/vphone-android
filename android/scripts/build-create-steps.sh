#!/usr/bin/env bash
# Build and test crate vphone-create-steps (android/create/installer).
#
#   android/scripts/build-create-steps.sh           # aarch64-linux-android, release
#   android/scripts/build-create-steps.sh test      # host tests (needs python3, gcc, Pillow and the
#                                                   # downloads below under android/build/create-steps)
#
# Host tests use, under android/build/create-steps: dropbear/src (dropbear 2016.74
# built from source), sshtars/{ssh.tar.gz,x} (the pinned sshtars), and
# release/{VPhone-2.14.2.zip,x} (vphone-cli's release); the real ramdisk tests use
# android/build/apfs-work/real.
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
# shellcheck disable=SC1091
. "$ROOT/android/env.sh"
export CARGO_TARGET_DIR=$ROOT/android/build/create-steps/target
cd "$ROOT/android/create/installer"
if [ "${1:-}" = test ]; then
  exec cargo test --release --no-fail-fast
fi
exec cargo build --release --target aarch64-linux-android
