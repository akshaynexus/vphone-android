# Source this: the pinned Rust and the NDK for aarch64-linux-android.
ANDROID_API=${ANDROID_API:-35}
export RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.cache/vphone-android/rustup}
export CARGO_HOME=${CARGO_HOME:-$HOME/.cache/vphone-android/cargo}
export PATH=$CARGO_HOME/bin:$PATH
export ANDROID_SDK_ROOT=${ANDROID_SDK:-$HOME/Android/Sdk}
export ANDROID_NDK_ROOT=${ANDROID_NDK_ROOT:-$ANDROID_SDK_ROOT/ndk/28.2.13676358}
NDK_BIN=$ANDROID_NDK_ROOT/toolchains/llvm/prebuilt/linux-x86_64/bin
export CC_aarch64_linux_android=$NDK_BIN/aarch64-linux-android$ANDROID_API-clang
export CXX_aarch64_linux_android=$NDK_BIN/aarch64-linux-android$ANDROID_API-clang++
export AR_aarch64_linux_android=$NDK_BIN/llvm-ar
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER=$CC_aarch64_linux_android
