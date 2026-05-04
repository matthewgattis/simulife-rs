#!/usr/bin/env bash
# Builds libviewer.so via cargo-ndk, drops it where Gradle expects it,
# then assembles the APK. Pass --release to build optimized binaries on
# both sides (default is debug for both).
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ABI="arm64-v8a"
RUST_TARGET="aarch64-linux-android"
JNILIBS_DIR="$PROJECT_ROOT/android/app/src/main/jniLibs/$ABI"

PROFILE="debug"
CARGO_PROFILE_FLAG=()
GRADLE_TASK="assembleDebug"
if [[ "${1:-}" == "--release" ]]; then
    PROFILE="release"
    CARGO_PROFILE_FLAG=(--release)
    GRADLE_TASK="assembleRelease"
fi

export JAVA_HOME="${JAVA_HOME:-/Applications/Android Studio.app/Contents/jbr/Contents/Home}"
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Library/Android/sdk}"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/30.0.14904198}"

echo "==> cargo ndk build (profile=$PROFILE, abi=$ABI)"
cd "$PROJECT_ROOT"
cargo ndk -t "$ABI" build -p viewer --lib ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"}

mkdir -p "$JNILIBS_DIR"
cp -f "target/$RUST_TARGET/$PROFILE/libviewer.so" "$JNILIBS_DIR/libviewer.so"
echo "==> copied libviewer.so to $JNILIBS_DIR"

echo "==> gradlew $GRADLE_TASK"
cd "$PROJECT_ROOT/android"
./gradlew "$GRADLE_TASK"

APK="$PROJECT_ROOT/android/app/build/outputs/apk/$PROFILE/app-$PROFILE.apk"
echo
echo "==> APK ready: $APK"
