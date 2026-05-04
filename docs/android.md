# Android build & deploy

The viewer crate cross-compiles to a `cdylib` (`libviewer.so`) for ARM64 Android
and is bundled into an APK by the minimal Gradle project under `android/`. The
`server` and `protocol` crates are unchanged on Android — only the viewer ships.

## One-time setup

You should already have these from the initial port; this section is a recovery
checklist if you set up a fresh machine.

- **Android Studio** (`brew install --cask android-studio`). Run the first-time
  wizard once. It installs the SDK to `~/Library/Android/sdk`.
- **NDK 30.0.14904198 + CMake** via Android Studio's SDK Manager → SDK Tools tab.
  (Standard install does not include the NDK — add it explicitly.)
- **Rust target**: `rustup target add aarch64-linux-android`
- **cargo-ndk**: `cargo install cargo-ndk`
- **Gradle** (only for bootstrapping the wrapper, which is already committed):
  `brew install gradle`. Once `android/gradlew` exists, system Gradle isn't used.

### Environment variables

In `~/.zshrc`:

```bash
export ANDROID_HOME="$HOME/Library/Android/sdk"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/30.0.14904198"
export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
export PATH="$ANDROID_HOME/platform-tools:$ANDROID_HOME/emulator:$PATH"
```

`JAVA_HOME` points at the JDK 21 bundled with Android Studio — Gradle uses it,
no separate JDK install needed.

## Build

From the repo root:

```bash
./android/build_apk.sh           # debug build
./android/build_apk.sh --release # release build (≈12 MB APK vs ≈57 MB debug)
```

The script:
1. Runs `cargo ndk -t arm64-v8a build -p viewer --lib [--release]` to produce
   `target/aarch64-linux-android/<profile>/libviewer.so`.
2. Copies the `.so` into `android/app/src/main/jniLibs/arm64-v8a/`.
3. Invokes `./gradlew assembleDebug` or `assembleRelease`.

Output APK lands at `android/app/build/outputs/apk/<profile>/app-<profile>.apk`.

Both profiles are signed with the Android debug keystore (`~/.android/debugkey`)
so they can be sideloaded via `adb install`. Don't ship these to the Play Store
— a real upload key is required for that.

## Devices

```bash
adb devices
```

Lists serials — typically the Galaxy S21 (`R5CR11PN1BB`) when connected by USB.
For multiple devices, pass `-s <serial>` to every adb command. The emulator
(`emulator-5554`) currently segfaults on wgpu init due to a buggy
`vulkan.ranchu.so` — use the physical device.

### Enabling USB debugging on the S21 (one-time)

Settings → About phone → Software information → tap **Build number** 7 times.
Then Settings → Developer options → toggle **USB debugging**. Plug in via USB,
approve the RSA prompt with **Always allow**.

If `adb devices` shows `unauthorized`, the prompt didn't appear — check the
phone. If it shows nothing, the USB mode is "Charging only"; pull down the
notification shade and switch to "File transfer / Android Auto".

## Install and launch

```bash
SERIAL=R5CR11PN1BB
APK=android/app/build/outputs/apk/release/app-release.apk

adb -s "$SERIAL" install -r "$APK"
adb -s "$SERIAL" shell am start -n net.iapetusservers.caviewer/android.app.NativeActivity
```

Force-stop:

```bash
adb -s "$SERIAL" shell am force-stop net.iapetusservers.caviewer
```

Uninstall:

```bash
adb -s "$SERIAL" uninstall net.iapetusservers.caviewer
```

## Logs

```bash
adb -s "$SERIAL" logcat -c                       # clear ring buffer
adb -s "$SERIAL" logcat | grep CAViewer          # live tail, filtered
adb -s "$SERIAL" logcat -d 2>&1 | grep CAViewer  # dump current
```

Tag is `CAViewer` for everything routed through our tracing → log bridge.
Crashes show up under `DEBUG` / `tombstoned`; if the process dies look for
`signal 11 (SIGSEGV)` in `logcat -d 2>&1 | grep -E "DEBUG|tombstoned"` along
with a backtrace.

## Server setup

The server runs on the dev Mac. The S21 must be on the same Wi-Fi.

```bash
cd ~/projects/cellular-automata
cargo run -p server --release -- \
  --listen 0.0.0.0:4433 \
  --world-width 6 --world-height 6
```

Important:
- `--listen 0.0.0.0:4433` (default `127.0.0.1` is loopback-only, the phone
  can't reach that).
- macOS will prompt for firewall permission on first run — click **Allow**.
- `--world-width`/`--world-height` are in chunks. Defaults are 36×24 — large
  for initial testing.

## Configuring the server address

The Android viewer has the server address hardcoded. Edit
`crates/viewer/src/lib.rs`:

```rust
const ANDROID_SERVER_ADDR: &str = "192.168.0.49:4433";
```

When the dev Mac's LAN IP changes (different network, DHCP renewal, etc.):

```bash
ipconfig getifaddr en0   # find the new IP
# edit crates/viewer/src/lib.rs
./android/build_apk.sh --release
adb -s "$SERIAL" install -r android/app/build/outputs/apk/release/app-release.apk
```

## One-shot rebuild + reinstall

```bash
./android/build_apk.sh --release && \
  adb -s R5CR11PN1BB shell am force-stop net.iapetusservers.caviewer && \
  adb -s R5CR11PN1BB install -r android/app/build/outputs/apk/release/app-release.apk && \
  adb -s R5CR11PN1BB shell am start -n net.iapetusservers.caviewer/android.app.NativeActivity
```

## Troubleshooting

**"INSTALL_FAILED_UPDATE_INCOMPATIBLE"** — signing config mismatch (e.g. you
installed a build from a different machine's debug keystore). Uninstall first:
`adb -s "$SERIAL" uninstall net.iapetusservers.caviewer`.

**App launches but immediately closes** — almost always a Vulkan/wgpu init
crash. Run `adb logcat -d 2>&1 | grep -E "DEBUG|tombstoned" | head -100` and
look for the top of the backtrace. Currently this happens reliably on the
emulator (`vulkan.ranchu.so`) and not on the S21.

**"Reconnecting…" stays red** — server isn't reachable. Check:
1. Server is running with `--listen 0.0.0.0:4433` (not `127.0.0.1`).
2. macOS firewall allowed the inbound connection.
3. Phone is on the same Wi-Fi as the dev Mac.
4. `ANDROID_SERVER_ADDR` matches `ipconfig getifaddr en0`.

**Gradle "SDK not found"** — `ANDROID_HOME` not exported in the shell that
invokes the build. Check with `echo $ANDROID_HOME` from the same terminal.
