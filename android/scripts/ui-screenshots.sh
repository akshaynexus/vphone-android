#!/bin/sh
# Draws the app's screens on this computer, with made-up state, in light and dark: no phone and
# no native library. The PNGs land in android/build/ui-screenshots/.
set -e
cd "$(dirname "$0")/.."
. ./env.sh
export ANDROID_HOME=$ANDROID_SDK_ROOT
mkdir -p build/tmp
rm -rf build/ui-screenshots
./gradlew --no-daemon --project-cache-dir build/.gradle :app:testDebugUnitTest --tests 'com.vphone.android.ScreenshotTest' "$@"
ls build/ui-screenshots
