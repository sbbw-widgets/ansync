# ansync — Claude instructions

Reescritura de scrcpy en Rust con scope ampliado: mirror de pantalla, control bidireccional, transferencia de archivos, cámara/micrófono virtuales, audio bidireccional, clipboard sync, descubrimiento mDNS, pairing seguro con Ed25519 + Noise XX sobre QUIC.

**Lee [`PLAN.md`](./PLAN.md) para el roadmap canónico, decisiones cerradas y estado de cada step.** Este archivo es la guía rápida para sesiones nuevas.

## Layout

```
crates/      librerías por dominio, todas con traits + impls detrás de feature flags
bins/        ansyncd (daemon + GUI eframe) + ansyncctl (CLI)
android/     companion app Kotlin (Gradle KTS)
nix/         módulos NixOS / home-manager y derivaciones de build
```

## Reglas duras

- **Traits primero**. Cada backend (`AudioBackend`, `VirtualCameraSink`, `VirtualInputDevice`, `Transport`, `Discovery`, `ClipboardBackend`, `PermissionsStore`) es un trait. Impls concretas detrás de feature flags.
- **Permisos por dispositivo**. Cualquier acción que toque hardware, red u IO chequea `DevicePermissions` antes de proceder. Sin flag = `Error::PermissionDenied(Permission)`. Persistencia: `$XDG_CONFIG_HOME/ansync/devices/{id}.toml`.
- **Sin ffmpeg**. Codecs vía `ferricast-encoder` / `ferricast-decoder` (NVENC, VAAPI, openh264).
- **Sin OpenSSL**. `rustls` con `default-features = false`, root store vacío, custom verifier que pinea al pubkey Ed25519 del peer.
- **Sin `#[allow(unused_*)]`**. Si algo no se usa, eliminarlo. Si la visibilidad rompe la signature pública, ajustar `pub(crate)` del módulo.
- **`tracing` → `tracing-journald`** en el daemon. Sin `println!` salvo en el CLI.
- **ADB siempre via `adb_client` crate** (protocolo TCP a `adbd` 127.0.0.1:5037). NUNCA `Command::new("adb")`. `device.shell_command(["pm", ...])` es la API correcta para `pm grant` / `am broadcast` / `dumpsys`.
- **Companion runtime perms**: cuando agregues un permiso "dangerous" nuevo a `AndroidManifest.xml`, sumá su nombre fully-qualified al array `COMPANION_RUNTIME_PERMS` en `crates/pairing/src/cable.rs`. Normal install-time perms (INTERNET, WAKE_LOCK, etc.) NO van. AppOps perms tampoco (los maneja el `SetupNotif` flow del companion).
- **Commits single-line**. Conventional (`feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `build:`, `ci:`). Sin Co-Authored-By, sin body salvo pedido explícito.

## Estilo Rust

- Edition 2024, stable
- Newtypes para identificadores (`DeviceId`, `SessionId`, `TransferId`)
- `Result<T, ansync_core::Error>` global; errores por crate envueltos en variantes
- `?` antes que `unwrap`/`expect` fuera de tests
- Traits sealed para sets cerrados, typestate cuando una entidad tiene fases distintas
- Generics + trait bounds antes que `dyn Trait` cuando monomorfización sirve

## Build

```sh
nix develop
cargo check --workspace
cargo build --workspace
```

El `flake.nix` pinea `nixpkgs` a `549bd84d6279f9852cae6225e372cc67fb91a4c1` para compartir cache con `/etc/nixos/flake.lock` del sistema.

## Estado actual

**Roadmap completo** — Steps 1-17 + todos los polish tracks (R, U, S, N) cerrados salvo N8/N9 (deferred).

### Qué funciona end-to-end

- **Mirror**: companion QSTile → MediaProjection → H.264 QUIC → daemon subprocess renderer (stdin/stdout postcard IPC). Mirror lifecycle clean: stream close → renderer shutdown. `install_logging(to_stderr)` evita que logs contaminen IPC pipe.
- **Input device→host**: uinput (Keyboard / Mouse / Touchscreen MT-B / Stylus / Gamepad XInput). `subframe_path` interpola posición en steps ≤6mm para evitar libinput jump-filter. `scaleTouchpadPressure` mantiene presión entre thresholds palm/touch de libinput.
- **Input host→device**: AccessibilityService `dispatchGesture` (TouchSlot desde egui coords).
- **Archivos**: Offer→Accept→Chunks 256 KiB→Complete + SHA-256 verify. `--download-dir` flag configurable. `ProtectHome` removido del systemd unit (sandbox bloqueaba escritura a `~/Downloads`).
- **Cámara**: v4l2loopback dinámico (`V4L2LOOPBACK_CTL_ADD`), card_label per-peer `"<Device> (Ansync)"`.
- **Audio**: PipeWire null-sink (`Audio/Sink` class, no `Source/Virtual`) + ByteRing (byte-level, sin chunk straddling) + quantum 960 alineado a Opus. Lazy sink provision desde `AudioStreamInit` header.
- **Clipboard**: Wayland bidi (text + image MIMEs), echo guard, auto-watcher sin GNOME (mutter no soporta `zwlr_data_control`).
- **Notificaciones**: `StreamKind::Notifications` → D-Bus signals `Device.NotificationPosted/Removed`.
- **Share**: Quick Share-style (`StreamKind::Url` + `StreamKind::Files`), `ShareActivity` intent-filter, xdg-open en host.
- **Pairing**: cable ADB (auto-install APK + pm grant) + WiFi PIN headless.
- **D-Bus**: `org.gameros.Ansync1`, Manager + Device + Permissions + PairingSession objects.
- **Companion headless**: QSTiles (Mirror/Mic/Camera/Touchpad/Share/Gamepad), HostDialer auto-connect con exponential backoff, BootReceiver, persistent notif state-driven.
- **NixOS module**: `services.ansync.{enable,user,package,extraGroups,quicPort,openFirewall,downloadDir}`. Firewall abierto por default.
- **CI/CD**: `workflows/ci.yml` (fmt+clippy+test+nix build) + `workflows/release.yml` (bundles Linux + APK). Attic cache en `cache.sergioribera.rs/main`.

### Regla sender-initiates

El trigger de cada stream vive en la punta que emite el dato:
- Screen mirror / camera / mic share → QSTiles del companion (privacidad del usuario)
- Audio sink (PC→phone) → D-Bus del host (`StartAudioSink` / `StopAudioSink`)
- Files / URLs → bidi por evento share
- Clipboard → always-sync

`ControlMessage` solo tiene `StartAudioSink` / `StopAudioSink`. Todos los demás `Start*` / `Stop*` anteriores fueron removidos.

### Pendiente / deferred

- **N8** — Multi-host companion (actualmente solo un host pareado)
- **N9** — Extensión Nautilus/Dolphin "Send to device"
- **snd_aloop dual-backend audio** — Discutido como fallback PipeWire, no implementado aún. Ver discusión en sesión 2026-07-06: pool de 8 substreams, bitmask slot allocation, `acquire()`/`release()` per peer via cpal `hw:Loopback,0,N`.

## Convenciones de continuidad

Al retomar en una sesión nueva:

1. Leer `PLAN.md` y este archivo.
2. Identificar el primer step sin `[x]` o el pendiente acordado con el usuario.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` en `PLAN.md`, actualizar esta sección "Estado actual", y commitear con un single-line.
