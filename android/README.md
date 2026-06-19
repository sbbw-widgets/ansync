# ansync companion (Android)

The mobile half of [ansync](../README.md). It runs in the background, finds
your paired PC on the same Wi-Fi, and powers everything the desktop side
asks for: screen mirroring, camera streaming, audio routing, file sharing,
clipboard sync, and notifications.

## You don't install this by hand

The companion is **installed automatically by `ansyncctl pair` on the PC**
the first time you connect a phone over USB. There is no Play Store entry,
no launcher icon, no app to open. Once paired, it stays running in the
background; you only see it as a small notification while a stream is
active.

If you want to side-load the APK manually (offline phones, kiosks, etc.)
grab the latest build from the
[Releases page](https://github.com/SergioRibera/ansync/releases) and
install with:

```sh
adb install -r -g companion-<version>.apk
```

The `-g` flag auto-grants runtime permissions; without it you'll need to
walk through the on-device setup notifications.

## Using it

Once paired, the companion exposes five **Quick Settings tiles** you can
drag into your tile tray:

| Tile        | What it does |
|-------------|--------------|
| Mirror to PC | Start / stop screen mirroring |
| Touchpad → PC | Use the phone as a trackpad for the PC |
| Share mic | Stream the phone's microphone to the PC |
| PC audio out | Hear the PC's audio on the phone |
| Send to PC | Pick any file and send it |

The system **Share sheet** (the standard Android share button) gets a
"Send via ansync" entry for files and links.

When the PC sends you a link, you'll see a heads-up notification asking
permission to open it. When the PC sends a file, you'll see a "tap to
open" notification once the transfer finishes.

That's the whole on-device surface.

## For developers

The companion is a tiny Kotlin app wrapping a Rust cdylib (`src/lib.rs`)
that handles QUIC, encryption, and the wire format — exactly the same code
the PC daemon uses, cross-compiled to Android via the NDK.

### Layout

```
android/
├── Cargo.toml           ← native cdylib `ansync_companion_native`
├── src/lib.rs           ← JNI surface (tokio + quinn client)
├── build.gradle.kts
├── gradle/libs.versions.toml
└── app/                 ← Kotlin module
    └── src/main/
        ├── AndroidManifest.xml
        ├── java/org/gameros/ansync/
        │   ├── NativeBridge.kt              ← JNI declarations
        │   ├── AnsyncCompanionService.kt    ← background service
        │   ├── AnsyncAccessibilityService.kt
        │   ├── tile/                        ← QSTile entry points
        │   ├── CaptureSession.kt            ← MediaProjection encoder
        │   ├── CameraSession.kt             ← Camera2 encoder
        │   ├── AudioRouter.kt               ← AudioRecord / AudioTrack
        │   ├── ClipboardBridge.kt
        │   ├── NotificationForwarder.kt
        │   ├── HostDialer.kt                ← LAN discovery + redial
        │   └── ShareActivity.kt             ← system share-sheet handler
        └── res/...
```

The Rust crate is **outside** the top-level workspace because it
cross-compiles for Android (`aarch64-linux-android`), while the host
workspace targets `x86_64-unknown-linux-gnu`. They share wire-format code
via `path = "../crates/proto"` + `path = "../crates/crypto"`, both
pure-Rust with no system dependencies.

### Build

```sh
# from the repo root, with the user's prebuilt rust-android docker image:
docker run --rm -v "$(pwd):/src" -w /src \
    rust-android:1.90-sdk-36 \
    -p android assembleDebug
```

The APK lands in `app/build/outputs/apk/debug/`.

The Gradle build wires `cargoBuild` ahead of `mergeJniLibFolders`, so a
normal `assembleDebug` / `assembleRelease` rebuilds the native library
automatically.

### Pins (`gradle/libs.versions.toml`)

| Component | Version |
|-----------|---------|
| Android Gradle Plugin | 8.13.0 |
| Kotlin | 2.0.20 |
| Compose BOM | 2024.02.00 |
| compileSdk | 36 |
| targetSdk | 34 |
| minSdk | 26 (Android 8.0 — `dispatchGesture` baseline) |
| NDK | 29.0.14033849 |
| Java toolchain | 17 |
| Mozilla rust-android plugin | 0.9.6 |

### Logs

```sh
adb logcat -s ansync ansync.svc ansync.camera ansync.audio \
              ansync.capture ansync.clip ansync.media \
              ansync.share ansync.keepalive
```
