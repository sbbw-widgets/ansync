# ansync — Plan & Roadmap

Documento canónico de decisiones y próximos pasos. Actualizar al cerrar cada step.

## Objetivo

Reescritura moderna de scrcpy en Rust con scope ampliado:

1. Mirror de pantalla Android → Linux con baja latencia
2. Control bidireccional (PC ↔ Android): teclado, mouse, touch, stylus, gamepad
3. Transferencia de archivos bidireccional + FUSE mount del FS Android
4. Cámara y micrófono virtuales en Linux usando el hardware del Android
5. Audio bidireccional con widgets de control en la barra de notificaciones Android
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
| Lenguaje Android | Kotlin, Gradle KTS últimas versiones |
| Build | Nix flake, crane, rust-overlay |
| Nixpkgs pin | `549bd84d6279f9852cae6225e372cc67fb91a4c1` (igual al sistema → cache compartido) |
| IPC | D-Bus session bus `org.gameros.Ansync1` vía `zbus` 5 |
| Service activation | systemd user unit (creado en Step 14) |
| Transporte | QUIC (`quinn`) + `rustls` (sin native roots), pinning a Ed25519 peer key |
| Discovery | mDNS (`mdns-sd`) |
| NAT traversal | NO MVP. Trait `Transport` abstrae para futuro relay/WireGuard |
| Pairing primario | Cable ADB one-shot (intercambio Ed25519). Después Wi-Fi puro |
| Pairing secundario | BT-HID para input-only (más adelante) |
| Crypto handshake | Noise XX vía `snow` |
| Identity | Ed25519 long-term, X25519 sessions |
| Proto | `postcard` + `serde`, versionado por `Envelope.version` |
| Codec video | H.264 default + H.265 cuando ambos peers tengan HW. NVENC → VAAPI → openh264 SW fallback |
| Codec audio | AAC (fdk-aac o symphonia SW fallback) + Opus opcional |
| AV1 | NO |
| ffmpeg | NUNCA — extender `ferricast` en su lugar |
| OpenSSL | NUNCA — rustls puro |
| GUI | `eframe` + `egui` + `wgpu` (parte del binario `ansyncd`) |
| Cámara virtual | trait `VirtualCameraSink`, impl inicial v4l2loopback con nombre = nombre del device |
| Audio | trait `AudioBackend`, impl inicial PipeWire (`pipewire-rs`) |
| Input host | trait `VirtualInputDevice`, impl inicial uinput (`input-linux`) |
| Input BT HID | crate `bluer`, perfil HID Device — secundario, no MVP |
| FS mount | trait `RemoteFsBackend`, impl FUSE3 (`fuser`) |
| Clipboard | trait `ClipboardBackend`, impl wayland (`wl-clipboard-rs`) + X11 fallback |
| Permisos | `DevicePermissions` por device, persistido en `$XDG_CONFIG_HOME/ansync/devices/{id}.toml` |
| Auto-mount | Sí, condicional a flag `files_mount` |
| Logs | `tracing` + `tracing-journald` |

## Permisos por dispositivo

Flags en `ansync_core::DevicePermissions`:

```
screen_mirror     camera_video      camera_audio      mic
audio_in          audio_out         files_send        files_receive
files_mount       clipboard_in      clipboard_out     input_from_device
input_to_device   notifications     sensors
```

Defaults al pairing:

- `screen_mirror`, `files_send`, `files_receive`, `notifications` → **on**
- `clipboard_in`, `clipboard_out` → **prompt**
- resto → **off** (usuario habilita explícito vía D-Bus / `ansyncctl perm`)

Cada syscall del daemon chequea el flag relevante antes de proceder. Sin flag → `Error::PermissionDenied(Permission::*)`.

## D-Bus surface

```
Service: org.gameros.Ansync1

Object /org/gameros/Ansync1/Manager
  Methods:
    ListDevices() → a(s)                       // device ids
    StartPairing(method: s) → o                // returns pairing session path
    ForgetDevice(id: s)
  Signals:
    DeviceAdded(id: s)
    DeviceRemoved(id: s)

Object /org/gameros/Ansync1/Device/{id}
  Properties (read-only):
    Id: s, Name: s, State: s, Capabilities: as,
    BatteryLevel: y, Address: s
  Methods:
    ShowScreen(), HideScreen()
    StartCamera(), StopCamera()
    StartMicrophone(), StopMicrophone()
    StartAudioRoute(direction: s)              // host-to-device | device-to-host | both
    SendFile(path: s) → o
    Mount(mountpoint: s), Unmount()
  Signals:
    StateChanged(state: s)
    BatteryChanged(level: y)
    IncomingFile(name: s, size: t)
    ClipboardRequest(content_preview: s)       // responder vía Permissions

Object /org/gameros/Ansync1/Permissions/{id}
  Methods:
    Get(flag: s) → b
    Set(flag: s, value: b)
    Reset()                                    // restaurar defaults
  Signals:
    PermissionChanged(flag: s, value: b)

Object /org/gameros/Ansync1/PairingPrompt
  Signals:
    PromptRequested(session_id: s, pin: s, qr_data: ay)
  Methods:
    Respond(session_id: s, accept: b)
  Fallback: si no hay listener al signal en 1500 ms → ansyncd spawnea diálogo egui local.
```

## Plan de inputs virtuales

**Host recibe input desde Android** (Android como teclado/touchpad/stylus/gamepad para PC):

- Crate `ansync-input` crea devices vía `uinput` con `input-linux`.
- Devices con nombre `Ansync {DeviceName} Keyboard/Stylus/...` para identificarlos en `libinput list-devices`.
- Tipos: Keyboard (evdev keymap full), Mouse (REL_X/Y + wheel + buttons), Touchscreen (MT-B multitouch hasta 10 dedos), Stylus (BTN_TOOL_PEN + ABS_X/Y/PRESSURE/TILT_X/TILT_Y), Gamepad (layout XInput-like).
- Capabilities negociadas en handshake — solo se crean devices que el peer soporta.

**Android recibe input desde host** (controlar pantalla espejeada):

- Companion app expone `AccessibilityService` (one-time grant) → `dispatchGesture()` para touch, `performGlobalAction()` para back/home, `InputConnection` para texto.
- Fallback con shell uid vía ADB para casos sin accessibility.

**Modo secundario BT-HID**:

- Crate `bluer`, perfil HID Device. Permite Android-as-keyboard/stylus sin companion en PC.
- No MVP — Step 13.

## Plan FUSE mount

- Crate `fuser` (FUSE3 puro Rust).
- Mount default: `$XDG_RUNTIME_DIR/ansync/mounts/{device-name}/`.
- Backend RPC sobre stream QUIC dedicado (`FsOp` en `ansync-proto`): `stat`, `readdir`, `open`, `read`, `write`, `create`, `unlink`, `rename`, `truncate`, `chmod`.
- Caches: dirent (TTL 5 s), inode metadata (TTL 5 s), readahead 256 KB. Writeback opcional (off por default).
- Anti-saturación:
  - Throttle: máx 4 requests in-flight por device (configurable).
  - Backpressure: batería <20 % o térmica alta → reduce a 1 in-flight + bloquea writes.
  - Companion Android usa SAF (Storage Access Framework) — usuario otorga acceso a carpetas específicas, no al FS completo.
- Privacy: primer acceso del host dispara prompt en Android. Permisos persistentes por carpeta.
- Auto-mount: al reconnect, si `files_mount=true` → monta. Si `false` → solo CLI explícito puede pedirlo (y aún así respeta el flag).

## Workspace layout

```
ansync/
├── flake.nix
├── flake.lock                  (generado al primer build)
├── Cargo.toml                  workspace
├── rust-toolchain.toml
├── CLAUDE.md
├── README.md
├── PLAN.md                     (este archivo)
├── crates/
│   ├── core/                   DeviceId, Capabilities, Permissions, Error
│   ├── proto/                  mensajes postcard + versionado
│   ├── crypto/                 Ed25519 identity + Noise XX handshake
│   ├── discovery/              trait Discovery + mdns-sd impl
│   ├── transport/              trait Transport + quinn/rustls impl
│   ├── pairing/                cable ADB bootstrap + Wi-Fi + BT
│   ├── video/                  wrap ferricast-decoder, render a wgpu texture
│   ├── audio/                  trait AudioBackend + PipeWire impl
│   ├── camera/                 trait VirtualCameraSink + v4l2loopback impl
│   ├── input/                  trait VirtualInputDevice + uinput + BT HID impls
│   ├── files/                  transfer protocol + trait RemoteFsBackend + FUSE impl
│   ├── clipboard/              trait ClipboardBackend + wayland/X11 impls
│   ├── permissions/            DevicePermissions store + D-Bus surface
│   ├── dbus/                   interfaces zbus + servidor + cliente lib
│   └── daemon-core/            orchestrator compartido entre bins
├── bins/
│   ├── ansyncd/                daemon + GUI eframe/wgpu
│   └── ansyncctl/              CLI control
├── android/                    companion Kotlin (Gradle KTS) — futuro
└── nix/
    ├── package.nix             build vía crane
    ├── module.nix              NixOS module
    └── hm-module.nix           home-manager module
```

## Roadmap

- [x] **Step 1** — Skeleton workspace + flake + crates con traits + Cargo wiring + docs
- [x] **Step 2** — `proto` + `crypto` + `transport` QUIC echo end-to-end con pinning Ed25519
- [x] **Step 3** — `discovery` mDNS + `pairing` cable bootstrap → llave Ed25519 persistida en `$XDG_DATA_HOME/ansync/peers/`
- [x] **Step 4** — `permissions` storage + `dbus` Manager + Device + Permissions interfaces + systemd user unit + journald
- [x] **Step 5** — Extender `ferricast-encoder/decoder` con HEVC (NVENC + VAAPI) + wirear `ansync_video`
- [x] **Step 6** — `video` decode + `ansyncd` egui window — screen mirror end-to-end H.264 → wgpu texture
- [x] **Step 7** — `input` uinput — Android como kbd/touch/stylus para PC + reverse para controlar Android vía AccessibilityService
  - [x] **7a** — Host `ansync_input::uinput` impls (Keyboard / Mouse / Touchscreen MT-B / Stylus / Gamepad XInput-like) detrás del feature `uinput`. Ships `bins/ansyncd/contrib/60-ansync-uinput.rules` + `nix/uinput.nix` partial module — Step 14 lo importa al módulo NixOS consolidado para que el install sea plug-and-play (kernel module + udev rule + nota de group `input`).
  - [ ] **7b** — Mensajes input en `ansync_proto` + stream QUIC dedicado + dispatch en `daemon-core` (permission `input_from_device` check antes de cualquier `send`)
    - [x] **7b-1** — `InputSession` orchestrator en `ansync_input` (lazy device construction, permission gate per-event, `InputDeviceFactory` trait + `UinputFactory` impl, `proto::InputMessage → InputEvent` mapping).
    - [x] **7b-2** — QUIC server bind en `daemon-core` + accept loop + peer auth contra `PeerStore` + stream demux para `StreamKind::Input` → `InputSession::dispatch`. Transport gana `Ed25519AnyPeerVerifier` (trait `TrustedPeers`) + `QuicTransport::bind_any`; identidad del peer se recupera post-handshake via `quinn::Connection::peer_identity()`. `DaemonConfig.listen_addr` configurable (default `0.0.0.0:0`); mDNS anuncia el puerto real. `Capabilities::INPUT_FROM_DEV` activa por default.
  - [x] **7c** — Companion Android scaffold: `android/` con Gradle KTS + version catalog (`gradle/libs.versions.toml`), AGP 8.5.2 / Kotlin 2.0.20 / compileSdk 35 / minSdk 26 / Java 17. `AndroidManifest.xml` declara INTERNET + ACCESS_NETWORK_STATE + CHANGE_WIFI_MULTICAST_STATE (mDNS) + FOREGROUND_SERVICE + FOREGROUND_SERVICE_MEDIA_PROJECTION + POST_NOTIFICATIONS. Tres componentes stub: `MainActivity` (Compose status screen), `AnsyncCompanionService` (foreground service, notification channel, FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION), `AnsyncAccessibilityService` (static handle pattern para que el companion service llame `dispatchGesture` en Step 7e). Wrapper jar excluido del repo — usuario corre `gradle wrapper` una vez antes del primer `./gradlew assembleDebug`.
  - [ ] **7d** — Companion: MediaProjection capture → MediaCodec H.264 → QUIC client via Rust NDK + JNI a `quinn` (mismo wire format que el daemon, cero compat overhead).
    - [x] **7d-1** — Cdylib `ansync_companion_native` en `android/Cargo.toml` (fuera del workspace host). JNI surface: `nativeInit / nativeOpenConnection / nativeSendVideoChunk / nativePollInputMessage / nativeClose`. Stubs OK; tokio runtime + android_logger live; sesión guarda host+port. Gradle integra vía Mozilla `rust-android-gradle 0.9.6` (`cargoBuild` task encadenada con `mergeJniLibFolders`). Pins repineados a la imagen `rust-android:1.90-sdk-36` (Kotlin 1.9.22 / AGP 8.13.0 / NDK 29 / Compose Compiler 1.5.10).
    - [x] **7d-2** — `ansync_companion_native` ahora dial real: identity Ed25519 load_or_generate en `{filesDir}/identity.key`, `QuicTransport::connect` con pinning contra `daemonPubkeyHex` (64 hex). Apre `StreamKind::Video` + `StreamKind::Input` al handshake. Recv-loop async pushea bytes a `mpsc::UnboundedSender`; `nativePollInputMessage` consume del receiver. Pure path deps a `ansync-{core,proto,crypto,transport}` — workspace own (`[workspace]` vacío en `android/Cargo.toml`) para no contaminar el resolver host.
    - [x] **7d-3** — `CaptureSession` Kotlin: `MediaProjection` + `VirtualDisplay` + `MediaCodec` AVC encoder (Baseline, 1080p60 default, 8 Mbps, 5 s I-frame interval). Drain thread dedicado lee `dequeueOutputBuffer` → `nativeSendVideoChunk(bytes, ptsUs)`. `AnsyncCompanionService` recibe `ACTION_START_CAPTURE` con el `MediaProjection.Intent`, levanta foreground (FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION) y arranca `CaptureSession`. `MainActivity` ofrece botón que dispara `MediaProjectionManager.createScreenCaptureIntent()` + arranca el service con el resultado.
  - [x] **7e** — `AnsyncAccessibilityService` poll loop dedicado en `HandlerThread`. Llama `NativeBridge.nativePollInputMessage()`, decodifica con `WireInputMessage.decode`, replays TouchSlot via `dispatchGesture` (16 ms stroke). Rust side `encode_for_kotlin` flat tag+payload binary; schema mirrored en `WireInputMessage` (Kotlin) y comentado en `lib.rs` para que cualquier cambio toque ambos lados. KeyPress + Gamepad / Mouse stubs (Gamepad+Mouse no aplican en Android; KeyPress se mapea a `performGlobalAction` en step posterior si se necesita).
- [ ] **Step 8** — `files` transfer push/pull (sin mount)
- [ ] **Step 9** — `files` FUSE mount + SAF integration Android side
- [ ] **Step 10** — `camera` v4l2loopback con device name = nombre del Android
- [ ] **Step 11** — `audio` PipeWire bidireccional + notification widget Android (MediaSession)
- [ ] **Step 12** — `clipboard` con privacy gates por device
- [ ] **Step 13** — `input` BT HID secundario vía `bluer`
- [ ] **Step 14** — Nix module (NixOS + home-manager) + `nix-bundle-app` integration + crane build derivation. Importar `nix/uinput.nix` (Step 7a). Considerar fragmento similar para v4l2loopback (Step 10) y FUSE3 group `fuse` (Step 9).
- [ ] **Step 15** — README detallado + docs site + binary releases

## Dependencias Cargo (workspace)

Centralizadas en `[workspace.dependencies]`. Cada crate referencia con `dep.workspace = true`.

Categorías:

- **runtime**: tokio, futures, async-trait
- **serde**: serde, postcard
- **error**: thiserror
- **logs**: tracing, tracing-subscriber, tracing-journald
- **utils**: bytes, bitflags, uuid, directories, toml
- **crypto**: ed25519-dalek, x25519-dalek, snow, rustls, rustls-pemfile, rand_core
- **transport**: quinn
- **discovery**: mdns-sd
- **ipc**: zbus
- **ui**: eframe, egui, wgpu (consumidos en Step 6)
- **audio**: pipewire (consumido en Step 11)
- **camera**: v4l (consumido en Step 10)
- **input**: input-linux, bluer (consumidos en Steps 7 / 13)
- **fs**: fuser (consumido en Step 9)
- **clipboard**: wl-clipboard-rs (consumido en Step 12)
- **cli**: clap
- **ferricast** (path deps `../../ferricast/crates/...`): ferricast-core, ferricast-encoder, ferricast-decoder — wired en Steps 5/6

## Convenciones de código

- Rust edition 2024
- `clippy::all` + `clippy::pedantic` deny en CI (excepciones puntuales con justificación)
- Newtypes para IDs (`DeviceId`, `SessionId`, `TransferId`, etc.)
- `Result<T, ansync_core::Error>` global, errores por crate envueltos en variantes
- `?` antes que `unwrap`/`expect` fuera de tests
- Traits sealed para sets cerrados
- Typestate cuando convenga (e.g., conexión `Disconnected` → `Handshaking` → `Authenticated` → `Active`)
- Sin `#[allow(unused_*)]` — eliminar el código muerto
- Sin ffmpeg, sin OpenSSL

## Convenciones de commits

- Single-line conventional (`feat:`, `fix:`, `chore:`, `refactor:`, `docs:`, `build:`, `ci:`)
- Sin Co-Authored-By trailer
- Sin body salvo pedido explícito

## Notas de continuidad

Al retomar en una sesión nueva:

1. Leer `PLAN.md` y `CLAUDE.md`.
2. Identificar el primer step sin `[x]`.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` acá, actualizar "Estado actual" en `CLAUDE.md`, commitear single-line.

### Step 1 — entregables (este commit)

- `flake.nix` con pin compartido
- `Cargo.toml` workspace con todos los miembros + `[workspace.dependencies]` centralizadas
- `rust-toolchain.toml` stable
- 15 crates en `crates/` con `Cargo.toml` + `src/lib.rs` (traits + types core, sin impls)
- 2 binarios en `bins/` con `Cargo.toml` + `src/main.rs` mínimo
- `.gitignore`
- `CLAUDE.md`, `README.md`, `PLAN.md`

### Step 2 — cerrado

Entregables:

- `proto::frame` — length-prefixed postcard framing (`write_frame`/`read_frame` + typed helpers + `MAX_FRAME_SIZE = 16 MiB`).
- `crypto`:
  - `IdentityKeypair::load_or_generate(path)` persistencia 0600 sobre seed Ed25519 de 32 bytes.
  - `PeerIdentity::device_id()` = primeros 16 bytes del pubkey Ed25519.
  - `NoiseXxSession` (`Noise_XX_25519_ChaChaPoly_BLAKE2s`) con `into_transport()` → `NoiseTransport` AEAD.
- `transport::quic`:
  - `QuicTransport::new(identity)` genera cert self-signed Ed25519 vía rcgen al construir bind/connect.
  - `pinning::Ed25519ServerVerifier` / `Ed25519ClientVerifier` parsean el SPKI con `x509-parser` y comparan contra el pubkey esperado.
  - Streams etiquetados por `StreamKind` (1 byte al inicio del stream).
  - TLS 1.3 only, ALPN `ansync/1`, mutual auth obligatorio.
- `ansyncctl identity {init|show}` lee/escribe `$XDG_DATA_HOME/ansync/identity.key`.
- Test e2e `crates/transport/tests/echo.rs`: dos endpoints en `127.0.0.1`, pinning Ed25519, Noise XX 3-way handshake sobre el control stream, hello cifrado + echo.

### Step 3 — cerrado

Entregables:

- `discovery::MdnsDiscovery` anuncia `_ansync._udp.local.` con TXT `id=<pubkey hex 64>`, `name=<utf8>`, `caps=<u32 hex>`. `browse()` devuelve un `Pin<Box<Stream<Item=DiscoveredDevice>>>` derivado del `Receiver` de mdns-sd.
- `pairing::store::PeerStore` persiste en `$XDG_DATA_HOME/ansync/peers/{device_id}.toml` con perms `0700` directorio + `0600` archivo. API `put/get/remove/list`. Escritura atómica vía `*.toml.tmp` + rename.
- `pairing::cable` define el protocolo cable sobre cualquier stream `AsyncRead + AsyncWrite`: `bootstrap_host` espera `PairingMessage::BootstrapHello` y responde `BootstrapAck`; `bootstrap_companion` simétrico. Cable assures security ⇒ sin PIN; caps quedan vacías hasta la primera conexión control.
- `pairing::pair_host_via_adb(serial, identity, name)` orquesta `adb reverse tcp:port tcp:port`, TCP listen, bootstrap, cleanup de la reverse, devuelve `StoredPeer`.
- `ansyncctl discover [--seconds N]` browse mDNS por N segundos (default 5).
- `ansyncctl pair [--serial …] [--name …]` auto-selecciona si hay 1 device adb, exige `--serial` si hay varios.

### Step 4 — cerrado

Entregables:

- `permissions::FilePermissionsStore` toml en `$XDG_CONFIG_HOME/ansync/devices/{id}.toml` con writes atómicos (tmp + rename), dir 0700 / files 0600. `check`/`load`/`save`/`delete` async. Helpers `parse_permission`/`apply_permission`/`permission_value` para bridging hacia D-Bus.
- `dbus::DaemonState` posee identity + peer store + permissions store + device name. Vive en el crate dbus para evitar el ciclo con `daemon-core`.
- `dbus::Manager`, `Device`, `PermissionsIface` con `#[interface]` de zbus 5. Manager.ListDevices/ForgetDevice wired contra `PeerStore` + `PermissionsStore`; StartPairing devuelve `NotSupported` (D-Bus pairing en step posterior). Device expone props read-only, métodos retornan `NotSupported` hasta que aterricen los pipelines de media. Permissions.Get/Set/Reset persisten via store.
- `dbus::serve(state)` claim `org.gameros.Ansync1`, registra Manager + un par Device/PermissionsIface por cada peer ya pareado. `register_device`/`unregister_device` exportados para el flujo de pairing futuro.
- `daemon-core::Daemon` carga identity, abre stores, anuncia mDNS, levanta dbus, bloquea en SIGTERM/SIGINT.
- `bins/ansyncd`: CLI con `--device-name --identity --peers-dir --permissions-dir`, `tracing-journald` activo.
- `bins/ansyncd/contrib/ansyncd.service`: user unit con sandboxing (`ProtectSystem=strict`, `ProtectHome=read-only`, `NoNewPrivileges`), journald stdout.

### Step 6 — cerrado

Entregables:

- `ansync_video`: `HostDecoder` ya no usa thread-local cache — la "última frame" vive en `Arc<Mutex<Option<CapturedFrame>>>` propiedad de la instancia, así el productor (decoder loop) y el consumidor (sink GUI) pueden vivir en tasks distintas. `DecodedFrame` ahora carga `stride` y diferencia `Bgra8` / `Rgba8`.
- `ansync_video::feed::AnnexBFile`: lector streaming de `.h264` / `.h265` Annex-B sobre `tokio::fs`. Detecta start-codes 3/4 bytes, agrupa NALs por Access Unit (AUD-delimited o primer VCL post-NAL no-VCL), expone `next_packet() -> AnnexBPacket`. Suficiente para alimentar al decoder en Step 6 sin companion Android.
- `ansyncd::mirror_window`: `eframe::run_native` con `Renderer::Wgpu`. `MirrorApp` peekea el slot compartido, convierte NV12 / I420 / BGRA / RGBA → `egui::ColorImage` (BT.601 limited range, Q8 integer math), `ctx.load_texture` lo sube al texture manager de egui (wgpu por debajo). El widget mantiene aspect ratio centrando la imagen.
- `ansyncd::mirror_window::run_play_file_loop`: bombea `AnnexBFile` → `HostDecoder::feed` → `take` → slot compartido, paced a ~30 fps. Falla limpio si `local_decoder_caps()` no soporta el codec.
- `bins/ansyncd` CLI: nuevo flag `--play-file PATH` + funciones `run_play_file_loop` / `spawn_play_file` detrás del feature **`dev-playback`** (off por default). El renderer (`MirrorApp`, conversión, `mirror_window::run`) queda como código prod sin gate porque el daemon lo necesita para servir `ShowScreen` desde D-Bus en Step 7. `ansyncd` se splittea en `[lib]` + `[[bin]]` (mismo name) para que los items `pub` del renderer no disparen `dead_code` hasta que Step 7 wire el caller prod. Con feature on se levanta solo la mirror window + decode loop (D-Bus / mDNS skip — Step 6 es path de test standalone). Step 14 (Nix derivation) tiene que dejar la feature off.
- `flake.nix`: `LIBCLANG_PATH` exportado para que `bindgen` (transitivo vía VA-API + NVDEC en ferricast) parsee headers dentro del shell de nix.

### Step 5 — cerrado

Entregables del lado ferricast:

- `ferricast-core` expone `H265Profile { Main, Main10 }` + `max_h265_profile` en `DeviceCapabilities` y `EncoderConfig`.
- `ferricast-encoder::nvenc::NvencEncoder<C>` generic sobre sealed `NvencCodec`; aliases `NvencH264Encoder` / `NvencH265Encoder`. Feature `nvenc-hevc` (default-off) habilita el marker HEVC.
- `ferricast-encoder::h265` agrega VAAPI HEVC encoder completo: bitstream + headers VPS/SPS/PPS + parameter buffers + packed headers. Feature `vaapi-hevc`.
- `H265Encoder` facade (NVENC → VAAPI, sin SW fallback) con `FERRICAST_H265_BACKEND` override.
- `ferricast-decoder::nvdec::NvdecDecoder<C>` generic con markers H.264 + HEVC; aliases `NvdecH264Decoder` / `NvdecH265Decoder`. Features `nvdec-decode` / `nvdec-hevc-decode`. NVDEC ahora vive en el chain del `H264Decoder` facade (NVDEC → VAAPI opt-in → openh264).
- `ferricast-decoder::h265` con `H265Decoder` facade (NVDEC → VAAPI) + `VaapiH265Decoder` scaffold (display + profile probe + surface pool; slice submission retorna error explícito, mismo patrón que el H.264 VAAPI decoder opt-in).

Entregables del lado ansync:

- `ansync/Cargo.toml` activa `ferricast-core` / `ferricast-encoder` / `ferricast-decoder` con feature set `["openh264","vaapi","nvenc","nvenc-hevc","vaapi-hevc"]` (encoder) y `["openh264-decode","nvdec-decode","nvdec-hevc-decode","vaapi-hevc-decode"]` (decoder).
- `ansync_video` con `CodecCapabilities`, `negotiate_codec(peer, local)`, `local_decoder_caps()` (one-shot HW probe cacheado), `HostDecoder` enum dispatch sobre `H264Decoder | H265Decoder`. Sin render — Step 6.
