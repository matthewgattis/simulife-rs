# Android build & deploy

The viewer crate cross-compiles to a `cdylib` (`libviewer.so`) for Android and
is bundled into an APK by the minimal Gradle project under `android/`. The
`server` and `protocol` crates are unchanged on Android — only the viewer ships.

Two ABIs are supported: `arm64-v8a` for physical devices and `x86_64` for the
emulator on an x86_64 host. These notes describe an Arch Linux dev box.

## One-time setup

- **Android Studio**. Run the first-time wizard once with the **Standard**
  install; it puts the SDK in `~/Android/Sdk` and includes `platform-tools`
  (which is where `adb` comes from) and the emulator.
- **NDK + CMake**. The Standard install does *not* include these. Add them via
  **SDK Manager → SDK Tools tab → check "Show Package Details"**, then select
  **NDK (Side by side)** version `30.0.14904198` and **CMake**. The NDK version
  is pinned by `ndkVersion` in `android/app/build.gradle.kts`; `build_apk.sh`
  reads that pin and warns if a different version is what's installed.
- **JDK 17+**. AGP 8.7.3 rejects anything older. On Arch:
  `sudo pacman -S jdk21-openjdk && sudo archlinux-java set java-21-openjdk`.
  Android Studio's bundled JDK at `/opt/android-studio/jbr` also works —
  `build_apk.sh` finds either one.
- **Rust targets**: `rustup target add aarch64-linux-android x86_64-linux-android`
- **cargo-ndk**: `cargo install cargo-ndk`

### Environment variables

None are required — `build_apk.sh` locates the JDK, SDK and NDK itself. Set
`JAVA_HOME`, `ANDROID_HOME` or `ANDROID_NDK_HOME` only to override that
detection.

Adding `platform-tools` to your `PATH` is still convenient so `adb` works
without a full path:

```bash
export PATH="$HOME/Android/Sdk/platform-tools:$HOME/Android/Sdk/emulator:$PATH"
```

## Build

From the repo root:

```bash
./android/build_apk.sh                      # debug, arm64-v8a (physical device)
./android/build_apk.sh --release            # release, arm64-v8a
./android/build_apk.sh --emulator           # debug, x86_64 (emulator)
./android/build_apk.sh --release --all-abis # release, both ABIs
```

The script:
1. Detects `JAVA_HOME` / `ANDROID_HOME` / `ANDROID_NDK_HOME` and echoes what it
   picked.
2. Wipes `android/app/src/main/jniLibs/` so an ABI from a previous run can't
   ride along in this APK.
3. Runs `cargo ndk -t <abi>… -o …/jniLibs build -p viewer --lib [--release]`,
   which writes each `libviewer.so` straight into the `<abi>/` layout Gradle
   expects.
4. Invokes `./gradlew assembleDebug` or `assembleRelease`.

Output APK lands at `android/app/build/outputs/apk/<profile>/app-<profile>.apk`.

On the first build, Gradle downloads its distribution and AGP dependencies, and
AGP auto-installs any missing SDK platform / build-tools it needs (the pinned
`compileSdk` platform won't necessarily be the one the Studio wizard installed).
Expect that run to take a few minutes; later ones are much faster.

Release builds are much smaller than debug (roughly 12 MB vs 57 MB for a
single-ABI build; a two-ABI debug APK runs about 65 MB). Both
profiles are signed with the Android debug keystore (`~/.android/debug.keystore`)
so they can be sideloaded with `adb install`. Don't ship these to the Play Store
— a real upload key is required for that.

## Physical device

```bash
adb devices
```

Lists serials — e.g. the Galaxy S21 as `R5CR11PN1BB` over USB. With more than
one device (including a running emulator) attached, pass `-s <serial>` to every
adb command.

### Enabling USB debugging (one-time)

Settings → About phone → Software information → tap **Build number** 7 times.
Then Settings → Developer options → toggle **USB debugging**. Plug in via USB
and approve the RSA prompt with **Always allow**.

If `adb devices` shows `unauthorized`, the prompt didn't appear — check the
phone. If it shows nothing, the USB mode is "Charging only"; pull down the
notification shade and switch to "File transfer".

### Install and launch

```bash
SERIAL=R5CR11PN1BB
APK=android/app/build/outputs/apk/release/app-release.apk

adb -s "$SERIAL" install -r "$APK"
adb -s "$SERIAL" shell am start -n net.iapetusservers.caviewer/android.app.NativeActivity
```

Force-stop / uninstall:

```bash
adb -s "$SERIAL" shell am force-stop net.iapetusservers.caviewer
adb -s "$SERIAL" uninstall net.iapetusservers.caviewer
```

## Emulator

The emulator needs an **x86_64** system image on an x86_64 host, and therefore
an APK built with `--emulator` (or `--all-abis`). An arm64-only APK will not
install on it — ARM translation exists for x86_64 images but is unreliable for
Vulkan/NDK apps like this one.

Hardware acceleration needs `/dev/kvm` to be accessible:

```bash
ls -l /dev/kvm     # should exist and be readable/writable by your user
```

### Creating an AVD

Easiest path is Android Studio: **More Actions → Virtual Device Manager → Create
Device**, pick a phone, and choose a system image with ABI **x86_64** (the
"Google APIs" flavour is fine). Studio downloads the image for you.

Then:

```bash
./android/build_apk.sh --emulator
emulator -list-avds
emulator -avd <name> &
adb -s emulator-5554 install -r android/app/build/outputs/apk/debug/app-debug.apk
adb -s emulator-5554 shell am start -n net.iapetusservers.caviewer/android.app.NativeActivity
```

### Known-good configuration

Verified working on the Arch dev box: AVD `Pixel_7`, system image
`android-35 / google_apis / x86_64`, `hw.gpu.mode=host`, 4096 MB RAM, on an
Intel Iris Xe (ADL GT2) host with Mesa's Vulkan driver. wgpu selects the Vulkan
backend inside the guest and the UI renders:

```
viewer::render: wgpu adapter selected adapter=Intel(R) Iris(R) Xe Graphics (ADL GT2) backend=Vulkan
```

The old macOS dev machine hit a reliable wgpu segfault on its emulator (a buggy
`vulkan.ranchu.so`) and had to use the physical device. That does not reproduce
on a Linux host with a real GPU — the emulator's gfxstream backend forwards
guest Vulkan to the host driver.

If the app does start and immediately die, it's almost always Vulkan init inside
wgpu. Make sure the AVD is on GPU mode `host`:

```bash
emulator -avd <name> -gpu host
```

Note that `-gpu swiftshader_indirect` is *not* a useful fallback here: it
provides software GLES but no guest Vulkan driver, which this app requires.

## Logs

```bash
adb -s "$SERIAL" logcat -c                       # clear ring buffer
adb -s "$SERIAL" logcat | grep CAViewer          # live tail, filtered
adb -s "$SERIAL" logcat -d 2>&1 | grep CAViewer  # dump current
```

Tag is `CAViewer` for everything routed through our tracing → log bridge.
Crashes show up under `DEBUG` / `tombstoned`; if the process dies, look for
`signal 11 (SIGSEGV)` in `logcat -d 2>&1 | grep -E "DEBUG|tombstoned"` along
with a backtrace.

## Server address

The Android build has the server address hardcoded in
`crates/viewer/src/lib.rs`:

```rust
const ANDROID_SERVER_ADDR: &str = "iapetusservers.net:4433";
```

It points at the public server, so no per-network configuration is needed for
normal use. The hostname is resolved once at startup — if the A record changes,
restart the app.

To point the device at a server on your dev box instead, edit that constant to
your LAN IP (`ip -4 addr show scope global`), rebuild, and reinstall:

```bash
cargo run -p server --release -- \
  --listen 0.0.0.0:4433 \
  --world-width 6 --world-height 6
```

- `--listen 0.0.0.0:4433` matters — the default `127.0.0.1` is loopback-only and
  the phone can't reach it.
- Allow inbound 4433/UDP through the host firewall if one is active.
- The phone must be on the same Wi-Fi network.
- `--world-width` / `--world-height` are in chunks; the defaults of 36×24 are
  large for initial testing.

An emulator reaches a server on the host machine at the special address
`10.0.2.2`, not `127.0.0.1`.

## One-shot rebuild + reinstall

```bash
SERIAL=R5CR11PN1BB
./android/build_apk.sh --release && \
  adb -s "$SERIAL" shell am force-stop net.iapetusservers.caviewer && \
  adb -s "$SERIAL" install -r android/app/build/outputs/apk/release/app-release.apk && \
  adb -s "$SERIAL" shell am start -n net.iapetusservers.caviewer/android.app.NativeActivity
```

## Troubleshooting

**`ERROR: JAVA_HOME is set to an invalid directory`** — you have a stale
`JAVA_HOME` exported. Unset it and let `build_apk.sh` detect a JDK, or point it
at a real JDK 17+.

**`INSTALL_FAILED_UPDATE_INCOMPATIBLE`** — signing config mismatch, e.g. a build
signed by a different machine's debug keystore is already installed. Uninstall
first: `adb -s "$SERIAL" uninstall net.iapetusservers.caviewer`.

**`INSTALL_FAILED_NO_MATCHING_ABIS`** — the APK doesn't carry the target's ABI.
Rebuild with `--emulator` for an x86_64 emulator, or without it for a physical
arm64 device.

**App launches but immediately closes** — almost always a Vulkan/wgpu init
crash. Run `adb logcat -d 2>&1 | grep -E "DEBUG|tombstoned" | head -100` and
look at the top of the backtrace.

**"Reconnecting…" stays red** — the server isn't reachable. Check that it's
running with `--listen 0.0.0.0:4433`, that the host firewall allows inbound
4433/UDP, that the device is on the same network, and that
`ANDROID_SERVER_ADDR` matches.

**Gradle "SDK not found"** — `build_apk.sh` couldn't locate the SDK. It prints
the paths it resolved at the top of every run; set `ANDROID_HOME` explicitly if
those look wrong.
