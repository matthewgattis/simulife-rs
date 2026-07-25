#!/usr/bin/env bash
# Builds libviewer.so via cargo-ndk, drops it where Gradle expects it,
# then assembles the APK.
#
#   ./android/build_apk.sh                      debug, arm64-v8a (physical device)
#   ./android/build_apk.sh --release            release, arm64-v8a
#   ./android/build_apk.sh --emulator           debug, x86_64 (emulator on an x86_64 host)
#   ./android/build_apk.sh --release --all-abis release, both ABIs
#
# SDK/NDK/JDK locations are auto-detected; set ANDROID_HOME, ANDROID_NDK_HOME
# or JAVA_HOME to override.
set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
JNILIBS_DIR="$PROJECT_ROOT/android/app/src/main/jniLibs"
GRADLE_FILE="$PROJECT_ROOT/android/app/build.gradle.kts"

die() { echo "error: $*" >&2; exit 1; }

# ---------------------------------------------------------------- arguments

PROFILE="debug"
GRADLE_TASK="assembleDebug"
CARGO_PROFILE_FLAG=()
ABIS=(arm64-v8a)

while [[ $# -gt 0 ]]; do
    case "$1" in
        --release)
            PROFILE="release"
            GRADLE_TASK="assembleRelease"
            CARGO_PROFILE_FLAG=(--release)
            ;;
        --emulator)  ABIS=(x86_64) ;;
        --all-abis)  ABIS=(arm64-v8a x86_64) ;;
        -h|--help)   sed -n '2,11p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)           die "unknown argument: $1 (try --help)" ;;
    esac
    shift
done

# Maps an Android ABI to the Rust target triple that produces it.
rust_target_for_abi() {
    case "$1" in
        arm64-v8a) echo "aarch64-linux-android" ;;
        x86_64)    echo "x86_64-linux-android" ;;
        *)         die "no Rust target mapping for ABI $1" ;;
    esac
}

# ------------------------------------------------------------- JDK / SDK / NDK

# Major version of the JDK rooted at $1, or nothing if it isn't a usable JDK.
java_major() {
    local java_bin="$1/bin/java"
    [[ -x "$java_bin" ]] || return 0
    "$java_bin" -version 2>&1 \
        | sed -n 's/^.* version "1\.\([0-9]*\).*$/\1/p;s/^.* version "\([0-9]*\)[.".].*$/\1/p' \
        | head -1
}

# AGP 8.7 needs JDK 17+, and the `java` on PATH is often older (Arch keeps JDK 8
# co-installed), so probe candidates rather than trusting it.
detect_java_home() {
    local candidates=()
    [[ -n "${JAVA_HOME:-}" ]] && candidates+=("$JAVA_HOME")
    # Whatever `java` on PATH resolves to, minus the trailing /bin/java.
    if command -v java >/dev/null 2>&1; then
        local resolved
        resolved="$(readlink -f "$(command -v java)" 2>/dev/null || true)"
        [[ -n "$resolved" ]] && candidates+=("${resolved%/bin/java}")
    fi
    candidates+=(
        /usr/lib/jvm/java-21-openjdk
        /usr/lib/jvm/java-17-openjdk
        /opt/android-studio/jbr
        "/Applications/Android Studio.app/Contents/jbr/Contents/Home"
    )

    local candidate major
    for candidate in "${candidates[@]}"; do
        [[ -d "$candidate" ]] || continue
        major="$(java_major "$candidate")"
        if [[ -n "$major" && "$major" -ge 17 ]]; then
            echo "$candidate"
            return 0
        fi
    done
    die "no JDK 17+ found. Install one and set JAVA_HOME."
}

detect_android_home() {
    local candidates=()
    [[ -n "${ANDROID_HOME:-}" ]] && candidates+=("$ANDROID_HOME")
    [[ -n "${ANDROID_SDK_ROOT:-}" ]] && candidates+=("$ANDROID_SDK_ROOT")
    candidates+=("$HOME/Android/Sdk" "$HOME/Library/Android/sdk")

    local candidate
    for candidate in "${candidates[@]}"; do
        [[ -d "$candidate/platform-tools" || -d "$candidate/platforms" ]] || continue
        echo "$candidate"
        return 0
    done
    die "Android SDK not found. Install it via Android Studio or set ANDROID_HOME."
}

# Prefer the NDK version pinned in build.gradle.kts so Gradle and cargo-ndk
# agree; fall back to the highest installed version.
detect_ndk_home() {
    local sdk="$1"
    if [[ -n "${ANDROID_NDK_HOME:-}" && -d "$ANDROID_NDK_HOME" ]]; then
        echo "$ANDROID_NDK_HOME"
        return 0
    fi

    local pinned
    pinned="$(sed -n 's/.*ndkVersion[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$GRADLE_FILE" | head -1)"
    if [[ -n "$pinned" && -d "$sdk/ndk/$pinned" ]]; then
        echo "$sdk/ndk/$pinned"
        return 0
    fi

    local newest
    newest="$(ls -1 "$sdk/ndk" 2>/dev/null | sort -V | tail -1)"
    [[ -n "$newest" ]] || die "no NDK under $sdk/ndk. Install one via Android Studio's SDK Manager (SDK Tools tab, enable 'Show Package Details')."
    [[ -n "$pinned" ]] && echo "warning: build.gradle.kts pins NDK $pinned but only $newest is installed" >&2
    echo "$sdk/ndk/$newest"
}

JAVA_HOME="$(detect_java_home)"
ANDROID_HOME="$(detect_android_home)"
ANDROID_NDK_HOME="$(detect_ndk_home "$ANDROID_HOME")"
export JAVA_HOME ANDROID_HOME ANDROID_NDK_HOME

echo "==> JAVA_HOME=$JAVA_HOME"
echo "==> ANDROID_HOME=$ANDROID_HOME"
echo "==> ANDROID_NDK_HOME=$ANDROID_NDK_HOME"

# ------------------------------------------------------------------ toolchain

command -v cargo-ndk >/dev/null 2>&1 || die "cargo-ndk not found. Install with: cargo install cargo-ndk"

INSTALLED_TARGETS="$(rustup target list --installed)"
CARGO_ABI_FLAGS=()
for abi in "${ABIS[@]}"; do
    target="$(rust_target_for_abi "$abi")"
    grep -qx "$target" <<<"$INSTALLED_TARGETS" \
        || die "Rust target $target not installed. Add it with: rustup target add $target"
    CARGO_ABI_FLAGS+=(-t "$abi")
done

# ---------------------------------------------------------------------- build

echo "==> cargo ndk build (profile=$PROFILE, abis=${ABIS[*]})"
cd "$PROJECT_ROOT"

# Wipe jniLibs so an ABI built by a previous run can't ride along in this APK.
rm -rf "$JNILIBS_DIR"
mkdir -p "$JNILIBS_DIR"

# cargo-ndk's -o writes straight into the <abi>/ layout Gradle expects.
cargo ndk "${CARGO_ABI_FLAGS[@]}" -o "$JNILIBS_DIR" \
    build -p viewer --lib ${CARGO_PROFILE_FLAG[@]+"${CARGO_PROFILE_FLAG[@]}"}

for abi in "${ABIS[@]}"; do
    [[ -f "$JNILIBS_DIR/$abi/libviewer.so" ]] \
        || die "expected $JNILIBS_DIR/$abi/libviewer.so but it wasn't produced"
    echo "==> staged $abi/libviewer.so"
done

echo "==> gradlew $GRADLE_TASK"
cd "$PROJECT_ROOT/android"
./gradlew "$GRADLE_TASK"

APK="$PROJECT_ROOT/android/app/build/outputs/apk/$PROFILE/app-$PROFILE.apk"
echo
echo "==> APK ready: $APK"
