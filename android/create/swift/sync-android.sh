#!/bin/bash
# Compose the Android source tree in android/build/create/tree: a copy of linux-create, its own
# sync-sources.sh over the pinned vphone-cli, then the Android layer in this directory.
# linux-create/ and the vphone-cli checkout are never modified.
set -euo pipefail
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
ROOT=$(cd "$here/../../.." && pwd)
WORK=$ROOT/android/build/create
V=$WORK/vphone-cli-2.14.2
PIN_TAG=2.14.2 PIN_REV=83197152137ddd2aad70eb121d6b88a9afbee36e
[ -d "$V/.git" ] || git clone --depth 1 --branch "$PIN_TAG" https://github.com/Lakr233/vphone-cli.git "$V"
[ "$(git -C "$V" rev-parse HEAD)" = "$PIN_REV" ] || { echo "vphone-cli checkout is not $PIN_REV" >&2; exit 1; }
tree=$WORK/tree
mkdir -p "$tree"
rsync -a --delete --exclude .build --exclude /Sources --exclude Shims/libarchive/lib "$ROOT/linux-create/" "$tree/"
VPHONE_CLI=$V "$tree/sync-sources.sh" >/dev/null

# The Android layer: whole-file replacements and additions over the composed tree.
[ -d "$here/overlay" ] && cp -r "$here/overlay/." "$tree/"
# Dependencies whose manifests need Android added: local copies at their pinned revisions.
vendor=$WORK/vendor-spm
vendor_dep() { # name url revision
  local d=$vendor/$1
  if [ ! -d "$d/.git" ] || [ "$(git -C "$d" rev-parse HEAD)" != "$3" ]; then
    rm -rf "$d"; mkdir -p "$vendor"
    git init -q "$d"; git -C "$d" remote add origin "$2"
    git -C "$d" fetch -q --depth 1 origin "$3"; git -C "$d" checkout -q FETCH_HEAD
  fi
  git -C "$d" checkout -q . # back to pristine before the edits.txt run
}
vendor_dep MachOKit https://github.com/p-x9/MachOKit.git f0c6565435659aeb23369810874578d5344d13b7
python3 "$here/androidize.py" "$tree" "$vendor"
[ -f "$here/Package.resolved" ] && cp "$here/Package.resolved" "$tree/Package.resolved" || rm -f "$tree/Package.resolved"
echo "android tree composed in $tree"
