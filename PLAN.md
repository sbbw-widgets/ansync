# ansync — Plan & Roadmap

Documento canónico de decisiones y estado del roadmap. Actualizar al cerrar cada step.

## Objetivo

Reescritura moderna de scrcpy en Rust con scope ampliado:

1. Mirror de pantalla Android → Linux con baja latencia
2. Control bidireccional (PC ↔ Android): teclado, mouse, touch, stylus, gamepad
3. Transferencia de archivos bidireccional
4. Cámara y micrófono virtuales en Linux usando el hardware del Android
5. Audio bidireccional
6. Clipboard sync configurable por dispositivo
7. Descubrimiento mDNS en LAN, sin cable
8. Pairing seguro: cable ADB one-shot → llave Ed25519 long-term
9. Cifrado E2E con QUIC + rustls + pinning a Ed25519 peer key

## Decisiones cerradas

| Tema | Decisión |
|---|---|
| Workspace root | `ansync` |
| Binarios | `ansyncd` (daemon + GUI unificados), `ansyncctl` (CLI) |
| Lenguaje host | Rust stable, edition 2024 |
| Lenguaje Android | Kotlin, Gradle KTS |
| Build | Nix flake, crane, rust-overlay |
| Nixpkgs pin | `549bd84d6279f9852cae6225e372cc67fb91a4c1` |
| IPC | D-Bus session bus `org.gameros.Ansync1` vía `zbus` 5 |
| Service activation | systemd user unit |
| Transporte | QUIC (`quinn`) + `rustls`, pinning a Ed25519 peer key |
| Discovery | mDNS (`mdns-sd`) |
| NAT traversal | NO MVP — trait `Transport` abstrae para futuro relay |
| Pairing primario | Cable ADB one-shot + WiFi PIN headless |
| Crypto handshake | Noise XX vía `snow` |
| Identity | Ed25519 long-term, X25519 sessions |
| Proto | `postcard` + `serde`, versionado por `Envelope.version` |
| Codec video | H.264 default + H.265 HW. NVENC → VAAPI → openh264 SW |
| Codec audio | Opus 48kHz/stereo/S16LE (PipeWire null-sink ByteRing) |
| ffmpeg | NUNCA — extender `ferricast` |
| OpenSSL | NUNCA — rustls puro |
| GUI | `eframe` + `egui` + `wgpu` subprocess renderer (`mirror-renderer`) |
| Cámara virtual | v4l2loopback dinámico (`V4L2LOOPBACK_CTL_ADD`), card_label per-peer |
| Audio | PipeWire null-sink (`Audio/Sink`) + ByteRing. cpal/snd_aloop fallback: pendiente |
| Input host | uinput (`input-linux`) + subframe interpolation para touchpad |
| Clipboard | `wl-clipboard-rs` Wayland + X11 fallback futuro |
| Permisos | `DevicePermissions` por device, `$XDG_CONFIG_HOME/ansync/devices/{id}.toml` |
| Logs | `tracing` + `tracing-journald` |
| Sender-initiates | Mirror/cam/mic → QSTile companion; AudioSink → D-Bus host |

## Permisos por dispositivo

Flags en `ansync_core::DevicePermissions`:

```
screen_mirror  camera_video  camera_audio  mic
audio_in       audio_out     files_send    files_receive
clipboard_in   clipboard_out input_from_device input_to_device
notifications  share_receive
```

Defaults al pairing: `screen_mirror`, `files_send/receive`, `notifications`, `clipboard_in/out`, `audio_in/out`, `share_receive` → **on**. Resto → **off**.

## D-Bus surface

```
Service: org.gameros.Ansync1

/org/gameros/Ansync1/Manager
  Methods: ListDevices() → a(s), BrowseAvailable(seconds) → a(sss),
           StartPairing(addr, pubkey_hex) → o, ForgetDevice(id), RefreshPeers()
  Signals: DeviceAdded(id), DeviceRemoved(id), DeviceConnectivityChanged(id, state),
           DeviceReachable(id, addr), DeviceUnreachable(id)

/org/gameros/Ansync1/Device/{id}
  Properties: Id, Name, State (Disconnected|Pairing|Authenticated|Active),
              Capabilities, BatteryLevel, Address
  Methods: StartAudioSink(), StopAudioSink(), SyncClipboard(),
           SendFiles(paths: as), SendUrl(url: s)
  Signals: StateChanged(state), BatteryChanged(level),
           NotificationPosted(app, title, body), NotificationRemoved(app, key),
           FileReceived(path), StreamStateChanged(kind, active)

/org/gameros/Ansync1/Permissions/{id}
  Methods: Get(flag) → b, Set(flag, value), Reset()
  Signals: PermissionChanged(flag, value)

/org/gameros/Ansync1/Pair/{uuid}
  Properties: State, HostName, HostPubkeyHex, Address, Error
  Methods: SubmitPin(pin), Cancel()
  Signals: Completed(device_id, name), Failed(reason)
```

## Workspace layout

```
ansync/
├── flake.nix / flake.lock / Cargo.toml / rust-toolchain.toml
├── crates/
│   ├── core/         DeviceId, Capabilities, Permissions, Error
│   ├── proto/        mensajes postcard + versionado
│   ├── crypto/       Ed25519 identity + Noise XX handshake
│   ├── discovery/    trait Discovery + mdns-sd impl
│   ├── transport/    trait Transport + quinn/rustls impl
│   ├── pairing/      cable ADB + WiFi PIN bootstrap
│   ├── video/        ferricast-decoder wrap, sink_egui, IPC mirror-renderer
│   ├── audio/        trait AudioBackend + PipeWire impl (cpal fallback TBD)
│   ├── camera/       trait VirtualCameraSink + v4l2loopback + dyn_ctl
│   ├── input/        trait VirtualInputDevice + uinput + subframe
│   ├── files/        transfer protocol (Offer→Chunks→Complete)
│   ├── clipboard/    trait ClipboardBackend + wayland impl + watcher
│   ├── permissions/  DevicePermissions store + parse/apply helpers
│   ├── dbus/         interfaces zbus + server + client lib
│   └── daemon-core/  orchestrator (accept loop, registries, action_loop)
├── bins/
│   ├── ansyncd/      daemon + mirror-renderer subcommand
│   └── ansyncctl/    CLI (pair, push, url, audio-sink-*, perm)
├── android/          companion Kotlin (Gradle KTS) + Rust cdylib JNI
└── nix/
    ├── package.nix   crane build (portable ? false)
    ├── module.nix    NixOS module (uinput + v4l2loopback + systemd user unit)
    ├── uinput.nix    kernel module + udev rule
    ├── v4l2loopback.nix  dynamic mode (devices=0)
    └── hm-module.nix home-manager fallback
```

## Roadmap

- [x] **Step 1** — Skeleton workspace + flake + crates con traits
- [x] **Step 2** — `proto` + `crypto` + `transport` QUIC echo end-to-end con pinning Ed25519
- [x] **Step 3** — `discovery` mDNS + `pairing` cable bootstrap → Ed25519 persistida
- [x] **Step 4** — `permissions` + `dbus` Manager/Device/Permissions + systemd unit + journald
- [x] **Step 5** — Extender `ferricast` con HEVC (NVENC + VAAPI) + wirear `ansync_video`
- [x] **Step 6** — `video` decode + `ansyncd` egui window — H.264 → wgpu texture
- [x] **Step 7** — `input` uinput + companion AccessibilityService input bidi
- [x] **Step 8** — `files` transfer push/pull (Offer→Accept→Chunks→Complete + SHA-256)
- [x] **Step 9** — ~~FUSE + SAF~~ DROPPED. File transfer Step 8 = surface oficial.
- [x] **Step 9.5** — Integration glue end-to-end (mirror + input + pairing + touchpad full)
- [x] **Step 10** — `camera` v4l2loopback dinámico, card_label per-peer
- [x] **Step 11** — `audio` bidireccional PipeWire ↔ AudioRecord/AudioTrack
- [x] **Step 12** — `clipboard` sync Wayland ↔ Android (text + image bidi)
- [x] **Step 13** — ~~BT-HID~~ DROPPED. uinput + TouchpadActivity cubren el caso.
- [x] **Step 14** — Nix module + crane derivation + NixOS/hm-module + flake outputs
- [x] **Step 15** — README + docs
- [x] **Step 16** — Pure-Rust ADB (`adb_client` 2.x, cero shell-out)
- [x] **Step 17** — APK auto-fetch desde GitHub releases + version check

## Polish completado

Todos cerrados. Highlights:

- **R8** — v4l2loopback `V4L2LOOPBACK_CTL_ADD` per-peer card_label dinámico
- **S2** — Mirror subprocess renderer con stdin/stdout postcard IPC; lifecycle mirror close → renderer shutdown
- **S6** — Raw adbd protocol para `reverse` (cero binario `adb`)
- **S7/S8** — Auto-reconnect liveness probe + Wi-Fi wake lock + partial wake lock opt-in
- **S12** — PipeWire mic share: lazy sink provision + `Audio/Sink` class + ByteRing + quantum 960
- **U1-U5** — Hello frame (hostnames), ConnState machine, pair diagnosis, headless companion, QSTiles, WiFi PIN pair, mDNS presence watcher
- **N3/N5** — WakeLock partial + MirrorMediaSession widget
- **Share** — Quick Share-style bidi (files + URLs), ShareActivity intent-filter
- **Touchpad** — `scaleTouchpadPressure` (palm false-positive fix) + `subframe_path` (jump-filter fix)
- **Firewall** — NixOS module abre UDP 47215 + 5353 por default
- **Sandbox** — Removido `ProtectHome=read-only` del systemd unit; `--download-dir` CLI flag

## Pendiente / deferred

- [ ] **N8** — Multi-host companion (actualmente un solo `PREF_HOST_PUBKEY_HEX`)
- [ ] **N9** — Extensión Nautilus/Dolphin "Send to device" (Python / `.desktop`)
- [ ] **Audio snd_aloop fallback** — Discutido 2026-07-06. cpal abre `hw:Loopback,0,N` directamente. Pool de 8 substreams con bitmask, `acquire()`/`release()` per peer. PipeWire sigue siendo primario; snd_aloop es fallback para sistemas sin PipeWire.

## Dependencias Cargo

Centralizadas en `[workspace.dependencies]`:

- **runtime**: tokio, futures, async-trait
- **serde**: serde, postcard
- **error**: thiserror
- **logs**: tracing, tracing-subscriber, tracing-journald
- **utils**: bytes, bitflags, uuid, directories, toml
- **crypto**: ed25519-dalek, x25519-dalek, snow, rustls, rand_core
- **transport**: quinn
- **discovery**: mdns-sd
- **ipc**: zbus
- **ui**: eframe, egui, wgpu (feature `dev-playback` / mirror-renderer)
- **audio**: pipewire (feature `pipewire-backend`), cpal (feature `cpal-backend`)
- **camera**: v4l (feature `v4l2loopback`)
- **input**: input-linux (feature `uinput`)
- **clipboard**: wl-clipboard-rs (feature `wayland`)
- **cli**: clap
- **ferricast** (git dep `db0f7531`): ferricast-core, ferricast-encoder, ferricast-decoder

## Convenciones de código

- Rust edition 2024, `clippy::all + pedantic` deny en CI
- Newtypes para IDs; `Result<T, ansync_core::Error>` global; `?` antes que `unwrap`
- Traits sealed para sets cerrados; typestate para fases de entidad
- Sin `#[allow(unused_*)]`; sin ffmpeg; sin OpenSSL

## Convenciones de commits

- Single-line conventional (`feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `build:`, `ci:`)
- Sin Co-Authored-By; sin body salvo pedido explícito

## Notas de continuidad

1. Leer `PLAN.md` y `CLAUDE.md`.
2. Identificar el primer step sin `[x]` o el pendiente acordado con el usuario.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` acá, actualizar "Estado actual" en `CLAUDE.md`, commitear single-line.
