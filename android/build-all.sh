#!/bin/bash
# Builds the Android debug APK from a clean checkout of this repository.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/scripts"
./apply-patches.sh
./build-deps.sh
./build-reims.sh
./build-qemu.sh
./build-turnip.sh
./build-apk.sh
