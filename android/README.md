# ansync companion (Android)

Companion app paired with the `ansyncd` host. Captures screen via
`MediaProjection`, encodes H.264 via `MediaCodec`, transports to the
host over QUIC (Rust `quinn`, same wire format as the daemon), and
replays remote input via an `AccessibilityService`.

## Layout

```
android/
├── Cargo.toml      ← native (Rust) cdylib `ansync_companion_native`
├── src/lib.rs      ← JNI surface (tokio runtime + quinn client)
├── settings.gradle.kts
├── build.gradle.kts
├── gradle.properties
├── gradle/libs.versions.toml
└── app/            ← Kotlin module
    ├── build.gradle.kts        ← applies Mozilla rust-android plugin
    └── src/main/
        ├── AndroidManifest.xml
        ├── java/org/gameros/ansync/
        │   ├── NativeBridge.kt          ← `external fun` surface
        │   ├── MainActivity.kt          ← Compose status screen
        │   ├── AnsyncCompanionService.kt← foreground service
        │   └── AnsyncAccessibilityService.kt
        └── res/...
```

The Rust crate lives **outside** the top-level `ansync` Cargo
workspace by design: the host workspace targets
`x86_64-unknown-linux-gnu`, while this crate cross-compiles against
the Android NDK. Sharing wire format goes through `path = "../crates/proto"`
+ `path = "../crates/crypto"` — both are pure-Rust + no system deps,
so they cross-compile cleanly.

## Build

Use the user's docker image `rust-android:1.90-sdk-36` (matches the
plugin / NDK / SDK pins in `gradle/libs.versions.toml`):

```sh
# From the ansync repo root:
docker run --rm -v "$(pwd):/src" -w /src \
    rust-android:1.90-sdk-36 \
    -p android assembleDebug
```

APK lands in `app/build/outputs/apk/debug/`.

The `cargoBuild` task is wired to depend on `mergeDebugJniLibFolders`
+ `mergeReleaseJniLibFolders`, so a normal `assembleDebug` /
`assembleRelease` rebuilds the native lib automatically.

## Pins (`gradle/libs.versions.toml`)

| Component | Version |
|---|---|
| Android Gradle Plugin | 8.13.0 |
| Kotlin | 1.9.22 |
| Compose Compiler Extension | 1.5.10 |
| Compose BOM | 2024.02.00 |
| compileSdk | 36 |
| targetSdk | 34 |
| minSdk | 26 (Android 8.0 — `dispatchGesture` available) |
| NDK | 29.0.14033849 |
| Java toolchain | 17 |
| Mozilla rust-android plugin | 0.9.6 |

## Step status

- **7c** — Gradle KTS scaffold + manifest + Kotlin stubs.
- **7d-1** (current) — Native Rust cdylib + JNI surface plumbed.
  `nativeInit` brings up the tokio runtime + android_logger;
  `nativeOpenConnection` / `nativeSendVideoChunk` /
  `nativePollInputMessage` are stubs that the Kotlin side can call
  without crashing.
- **7d-2** — Real `quinn` dial + Ed25519 verifier + `StreamKind::Video`
  push + `StreamKind::Input` recv.
- **7d-3** — Kotlin side: MediaProjection capture surface → MediaCodec
  H.264 → `NativeBridge.nativeSendVideoChunk`.
- **7e** — `AccessibilityService.dispatchGesture` consumes
  `nativePollInputMessage` and replays it on this device.
