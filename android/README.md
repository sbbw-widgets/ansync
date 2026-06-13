# ansync companion (Android)

Companion app paired with the `ansyncd` host. Captures screen via
`MediaProjection`, encodes H.264 via `MediaCodec`, transports to the
host over QUIC, and replays remote input via an `AccessibilityService`.

## Build

```sh
cd android
# First run only — fetches wrapper:
gradle wrapper --gradle-version 8.10
./gradlew assembleDebug
```

APK lands in `app/build/outputs/apk/debug/`.

## Step status

- **7c** (current) — Gradle KTS scaffold + manifest + service /
  activity stubs. `gradlew assembleDebug` should succeed; the app
  launches but only shows a status screen.
- **7d** (next) — QUIC client + MediaProjection capture + MediaCodec
  encode wired into `AnsyncCompanionService`.
- **7e** — `AccessibilityService.dispatchGesture` wired so the host
  can replay touch / key events.

## Pins (`gradle/libs.versions.toml`)

| Component | Version |
|---|---|
| Android Gradle Plugin | 8.5.2 |
| Kotlin | 2.0.20 |
| compileSdk / targetSdk | 35 |
| minSdk | 26 (Android 8.0 — `dispatchGesture` available) |
| Java toolchain | 17 |
| Compose BOM | 2024.08.00 |
