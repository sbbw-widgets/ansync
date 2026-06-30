# ansync — Plan & Roadmap

Documento canónico de decisiones y próximos pasos. Actualizar al cerrar cada step.

## Objetivo

Reescritura moderna de scrcpy en Rust con scope ampliado:

1. Mirror de pantalla Android → Linux con baja latencia
2. Control bidireccional (PC ↔ Android): teclado, mouse, touch, stylus, gamepad
3. Transferencia de archivos bidireccional
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
| Clipboard | trait `ClipboardBackend`, impl wayland (`wl-clipboard-rs`) + X11 fallback |
| Permisos | `DevicePermissions` por device, persistido en `$XDG_CONFIG_HOME/ansync/devices/{id}.toml` |
| Logs | `tracing` + `tracing-journald` |

## Permisos por dispositivo

Flags en `ansync_core::DevicePermissions`:

```
screen_mirror     camera_video      camera_audio      mic
audio_in          audio_out         files_send        files_receive
clipboard_in      clipboard_out     input_from_device input_to_device
notifications
```

Defaults al pairing:

- `screen_mirror`, `files_send`, `files_receive`, `notifications`,
  `clipboard_in`, `clipboard_out`, `audio_in`, `audio_out`,
  `share_receive` → **on**
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
│   ├── input/                  trait VirtualInputDevice + uinput impl
│   ├── files/                  transfer protocol
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
  - [x] **7b** — Mensajes input en `ansync_proto` + stream QUIC dedicado + dispatch en `daemon-core` (permission `input_from_device` check antes de cualquier `send`)
    - [x] **7b-1** — `InputSession` orchestrator en `ansync_input` (lazy device construction, permission gate per-event, `InputDeviceFactory` trait + `UinputFactory` impl, `proto::InputMessage → InputEvent` mapping).
    - [x] **7b-2** — QUIC server bind en `daemon-core` + accept loop + peer auth contra `PeerStore` + stream demux para `StreamKind::Input` → `InputSession::dispatch`. Transport gana `Ed25519AnyPeerVerifier` (trait `TrustedPeers`) + `QuicTransport::bind_any`; identidad del peer se recupera post-handshake via `quinn::Connection::peer_identity()`. `DaemonConfig.listen_addr` configurable (default `0.0.0.0:0`); mDNS anuncia el puerto real. `Capabilities::INPUT_FROM_DEV` activa por default.
  - [x] **7c** — Companion Android scaffold: `android/` con Gradle KTS + version catalog (`gradle/libs.versions.toml`), AGP 8.5.2 / Kotlin 2.0.20 / compileSdk 35 / minSdk 26 / Java 17. `AndroidManifest.xml` declara INTERNET + ACCESS_NETWORK_STATE + CHANGE_WIFI_MULTICAST_STATE (mDNS) + FOREGROUND_SERVICE + FOREGROUND_SERVICE_MEDIA_PROJECTION + POST_NOTIFICATIONS. Tres componentes stub: `MainActivity` (Compose status screen), `AnsyncCompanionService` (foreground service, notification channel, FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION), `AnsyncAccessibilityService` (static handle pattern para que el companion service llame `dispatchGesture` en Step 7e). Wrapper jar excluido del repo — usuario corre `gradle wrapper` una vez antes del primer `./gradlew assembleDebug`.
  - [ ] **7d** — Companion: MediaProjection capture → MediaCodec H.264 → QUIC client via Rust NDK + JNI a `quinn` (mismo wire format que el daemon, cero compat overhead).
    - [x] **7d-1** — Cdylib `ansync_companion_native` en `android/Cargo.toml` (fuera del workspace host). JNI surface: `nativeInit / nativeOpenConnection / nativeSendVideoChunk / nativePollInputMessage / nativeClose`. Stubs OK; tokio runtime + android_logger live; sesión guarda host+port. Gradle integra vía Mozilla `rust-android-gradle 0.9.6` (`cargoBuild` task encadenada con `mergeJniLibFolders`). Pins repineados a la imagen `rust-android:1.90-sdk-36` (Kotlin 1.9.22 / AGP 8.13.0 / NDK 29 / Compose Compiler 1.5.10).
    - [x] **7d-2** — `ansync_companion_native` ahora dial real: identity Ed25519 load_or_generate en `{filesDir}/identity.key`, `QuicTransport::connect` con pinning contra `daemonPubkeyHex` (64 hex). Apre `StreamKind::Video` + `StreamKind::Input` al handshake. Recv-loop async pushea bytes a `mpsc::UnboundedSender`; `nativePollInputMessage` consume del receiver. Pure path deps a `ansync-{core,proto,crypto,transport}` — workspace own (`[workspace]` vacío en `android/Cargo.toml`) para no contaminar el resolver host.
    - [x] **7d-3** — `CaptureSession` Kotlin: `MediaProjection` + `VirtualDisplay` + `MediaCodec` AVC encoder (Baseline, 1080p60 default, 8 Mbps, 5 s I-frame interval). Drain thread dedicado lee `dequeueOutputBuffer` → `nativeSendVideoChunk(bytes, ptsUs)`. `AnsyncCompanionService` recibe `ACTION_START_CAPTURE` con el `MediaProjection.Intent`, levanta foreground (FOREGROUND_SERVICE_TYPE_MEDIA_PROJECTION) y arranca `CaptureSession`. `MainActivity` ofrece botón que dispara `MediaProjectionManager.createScreenCaptureIntent()` + arranca el service con el resultado.
  - [x] **7e** — `AnsyncAccessibilityService` poll loop dedicado en `HandlerThread`. Llama `NativeBridge.nativePollInputMessage()`, decodifica con `WireInputMessage.decode`, replays TouchSlot via `dispatchGesture` (16 ms stroke). Rust side `encode_for_kotlin` flat tag+payload binary; schema mirrored en `WireInputMessage` (Kotlin) y comentado en `lib.rs` para que cualquier cambio toque ambos lados. KeyPress + Gamepad / Mouse stubs (Gamepad+Mouse no aplican en Android; KeyPress se mapea a `performGlobalAction` en step posterior si se necesita).
- [x] **Step 8** — `files` transfer push/pull (sin mount)
  - `ansync_files::transfer` con `send_file` / `receive_file` state machines. Offer (sha256) → Accept/Reject → Chunks (256 KiB) → Complete. Receiver verifica sha256 + size pre-ack. `InboundPolicy` trait desacopla destino del recv loop; `AutoAcceptPolicy` dumpea a `{root}/{peer_id}/{safe_name}` (alphanum + `.- _` allowed, resto `_`).
  - Permission gates: `Permission::FilesSend` para outbound (re-chequeado entre chunks → revoke mid-transfer surface clean), `FilesReceive` para inbound.
  - Daemon: `StreamKind::Files` accept loop spawnea `files_stream_loop` por stream con `AutoAcceptPolicy { root: download_dir }`. `DaemonConfig.download_dir` configurable (default `$XDG_DATA_HOME/ansync/incoming/`). `Capabilities::FILES` activa por default.
  - `ansyncctl push <id> <path> [--addr host:port] [--seconds N]`: direct QUIC dial (bypass D-Bus por ahora), discovery vía mDNS si `--addr` ausente, abre `StreamKind::Files`, `send_file`.
  - Companion: `files_accept_loop` en cdylib spawnea recv tasks por inbound stream del daemon. `PermissivePermissions` in-memory store ("everything on" — pairing ya estableció trust). Inbound files land en `{filesDir}/incoming/{peer_id}/{name}`.
- [x] **Step 9** — ~~FUSE mount + SAF integration~~ **DROPPED 2026-06-19**. Toda la FS RPC + FUSE mount + SAF server retirados (proto `FsOpMessage`/`FsMeta`/`FsEntry`, `ansync_files::fs`, `nix/fuse.nix`, `AnsyncFsServer.kt`, `FsOpCodec.kt`, `GrantStorageActivity.kt`, JNI fs methods). File transfer push/pull (Step 8) sigue siendo el surface oficial para mover archivos.
- [x] **Step 9.5** — Integration glue (end-to-end demoable)
  - Post-9.5 gap-closers:
    - **D-Bus dynamic registration**: `Manager.RefreshPeers()` method en `ansync_dbus::Manager`. `ansyncctl pair` lo llama post-`store.put` via `zbus::Proxy` session bus. Daemon ya corriendo registra el nuevo `/Device/{id}` + `/Permissions/{id}` sin restart. Idempotente — chequea `interface::<Device>` antes de re-attach.
    - **Companion Connect button + mDNS poll**: `HostDiscovery.kt` wrappea `NsdManager.discoverServices("_ansync._udp.")` con `WifiManager.MulticastLock` (sin lock Android Wi-Fi stack droppea multicast). MainActivity `DisposableEffect` arranca/para discovery; surface `discovered: List<DiscoveredHost>`. Cuando el paired pubkey hex matchea un host descubierto, aparece botón "Connect to X (IP)" que llama `nativeOpenConnection(addr, port, hex)`.
    - **Auto-install APK durante pair**: `pair_host_via_adb(serial, identity, name, apk: Option<&Path>)`. Verifica `adb shell pm list packages org.gameros.ansync` — si ausente, `adb install -r -g <apk>` (replace + auto-grant runtime perms). `ansyncctl pair --apk` o `$ANSYNC_COMPANION_APK` o `/usr/share/ansync/companion.apk` (orden). Mismo UX que scrcpy (que pushea `.jar` a `/data/local/tmp` + corre vía `app_process`, restricted a shell-uid hidden APIs).
  - [x] **9.5a** — Renderer movido de `ansyncd::mirror_window` a `ansync_video::sink_egui` (`MirrorApp`, `FrameSlot`, `run`, conversiones NV12/I420/BGRA/RGBA → ColorImage). Daemon-core `video_stream_loop` recibe stream `StreamKind::Video`, decode vía `HostDecoder`, pushea `DecodedFrame` al `FrameSlot` del peer en el `MirrorRegistry`. Slot per-peer sobrevive reconnects.
  - [x] **9.5b** — `DaemonAction` enum (`ShowScreen`/`HideScreen`) + `UnboundedSender` en `DaemonState`. `Device.ShowScreen`/`HideScreen` D-Bus methods envían action. `action_loop` en daemon-core consume y spawn-ea thread con `ansync_video::sink_egui::run(title, slot)` por peer. Window thread es separado del runtime tokio para no bloquear async.
  - [x] **9.5c** — Companion `streams_accept_loop` ahora maneja `StreamKind::Input` inbound del daemon → spawn `input_recv_loop` que pushea encoded events al mismo mpsc que consume `nativePollInputMessage` → AccessibilityService.dispatchGesture (7e). Convención clarificada en comentario: opener escribe, accepter lee. `nativeOpenConnection` ya no pre-abre Input stream.
  - [x] **9.5d** — Daemon `action_loop::ShowScreen` ahora abre `StreamKind::Input` outbound antes de spawn-ear el window thread. Wirea `UnboundedSender<InputMessage>` al `MirrorApp`. `input_writer_loop` consume del channel y postcard + write_frame al stream. `MirrorApp` emite `InputMessage::TouchSlot` mapeando pointer egui → coordenadas remotas (`fw × fh` desde último frame). Press/release/move/hover-exit emiten tracking_id 1/-1 standard MT-B.
  - [x] **9.5e** — `TouchpadActivity` Compose full-screen captura `MotionEvent` con `pointerInteropFilter` → `WireInputMessage.encode()` tag-binary → `nativeSendInputMessage(blob)`. Native `decode_input_from_kotlin` mirror del decoder Kotlin → `postcard::to_allocvec(InputMessage)` → write_frame. Outbound Input stream lazy-opens en `ActiveSession.outbound_input` la primera vez. Touch-down → `MouseButton{1,true}`, move → `MouseMove{dx,dy}` deltas, up/cancel → `MouseButton{1,false}`. Botón en MainActivity "Open touchpad".
  - [x] **9.5e+** — Device→host input completo: TouchpadActivity gana long-press → `MouseButton{2}` (right), 2-finger drag → `MouseWheel`, 2-finger tap → `MouseButton{3}` (middle), `TOOL_TYPE_STYLUS` → `Stylus` tag (x/y escaladas a 0..32767, pressure 0..8191, tiltX/tiltY desde `AXIS_TILT`+orient), `dispatchKeyEvent` hardware kbd → `KeyPress` evdev, IME `BasicTextField` con onValueChange sintetiza `KeyPress` por char (auto-shift para mayúsculas / punctuación ASCII). Nueva `GamepadActivity` overrides `dispatchKeyEvent` + `dispatchGenericMotionEvent` (SOURCE_JOYSTICK) → `Gamepad{buttons,lx,ly,rx,ry,lt,rt}`; QSTile `GamepadTile` lanzador. `KeycodeMap.kt` traduce `KeyEvent.KEYCODE_*` → evdev `KEY_*`. WireInputMessage Kotlin encode arms Stylus/Gamepad ya no tiran, mirror exacto del layout `encode_for_kotlin`. Rust `decode_input_from_kotlin` cubre tags 5 + 6 con nuevos helpers `take_u16` / `take_i16`.
  - [x] **9.5f** — Cable pairing companion side:
    - `pair_host_via_adb` ahora dispara `adb shell am broadcast -a org.gameros.ansync.action.PAIR -n org.gameros.ansync/.PairingReceiver --ei port $PORT --es name $HOST` después del `adb reverse`. Auto-wake del companion — no requiere abrir app primero.
    - `PairingReceiver` (manifest `<receiver exported=true>`) extrae port, llama `nativeInit + nativePairOverCable(port, deviceName)`.
    - Native `nativePairOverCable`: TCP connect 127.0.0.1:port + `bootstrap_companion` + return `"hex|name"`.
    - `PairingReceiver` persiste host pubkey + name en SharedPreferences (`PREF_HOST_PUBKEY_HEX` + `PREF_HOST_NAME`). Sin AlertDialog: cable es security guarantee (per cable.rs design intent).
    - `MainActivity` muestra "paired host: X (hex…)" si está pareado.
- [x] **Step 10** — `camera` v4l2loopback con device name = nombre del Android + D-Bus control (camera_id, w/h, fps, bitrate, codec, aspect, stabilization)
  - `ansync_proto::CameraConfig` (camera_id, w/h, fps, bitrate_kbps, codec, aspect, stabilization) + `CameraAspect` enum. `ControlMessage::StartCamera(CameraConfig)` reemplaza la variante stub. `StreamKind::Camera` (tag 0x07) en `transport`.
  - `ansync_camera::v4l2loopback::V4l2LoopbackSink` implementa `VirtualCameraSink` (feature `v4l2loopback`). Auto-discover scan `/dev/video*` por `V4L2_CAP_VIDEO_OUTPUT` o pin manual via `with_path`. set_format con FourCC NV12 (default) / YUYV / MJPG; `write_frame` via `libc::write` directo al fd (v4l2loopback acepta `write(2)`). Card label "Ansync" se setea via module param (nix/v4l2loopback.nix).
  - D-Bus `Device.StartCamera(camera_id, width, height, fps, bitrate_kbps, codec, aspect, stabilization)` + `StopCamera()`. Codec accepts `h264|h265`, aspect accepts `crop|letterbox|stretch`.
  - `DaemonAction::{StartCamera{device,config},StopCamera{device}}`. `daemon-core::action_loop`: chequea `Permission::CameraVideo`, abre `StreamKind::Control` outbound, envía postcard `Envelope{Message::Control(StartCamera)}`, spawn-ea `camera_decode_loop` (HostDecoder NV12 → V4l2LoopbackSink). `CameraRegistry` + `CameraEntry` per peer (sink+handle+frame_tx slots). Accept `StreamKind::Camera` inbound → empuja bytes al `frame_tx` del entry. Disconnect tear-down completo.
  - `Capabilities::CAMERA_VIDEO` activa por default en `DaemonConfig`.
  - Companion native: nuevos JNI `nativePollCameraControl` (poll inbound Control → tag-binary blob) + `nativeSendCameraChunk` (lazy-open outbound Camera stream) + `nativeStopCameraStream`. `streams_accept_loop` ahora demuxa `StreamKind::Control` → `control_recv_loop` decoda Envelope/Message + reencoda como tag-binary para Kotlin.
  - Companion Kotlin: `CameraSession` Camera2 + MediaCodec AVC/HEVC encoder con Surface input (zero-copy del sensor al encoder). `pickOutputSize` busca el sensor size mínimo ≥ target, fallback al más grande. `CONTROL_AE_TARGET_FPS_RANGE` + `CONTROL_VIDEO_STABILIZATION_MODE_ON` cuando `cfg.stabilization`. `AnsyncCompanionService` arranca `HandlerThread` "ansync-cam-ctrl" que polea + dispatcha Start/Stop. `WireCameraControl.kt` espejo del encoder Rust (tag 0 StartCamera, tag 1 StopCamera). Manifest gana `CAMERA` + `FOREGROUND_SERVICE_CAMERA`; service.foregroundServiceType = `mediaProjection|camera`.
  - `nix/v4l2loopback.nix` parcial: `extraModulePackages` + modprobe options (`devices=1 video_nr=10 card_label="Ansync" exclusive_caps=1`) + udev rule grupo `video`. Step 14 lo importa.
- [x] **Step 11** — `audio` bidireccional + Android AudioRecord/AudioTrack
  - `ansync_audio::CpalBackend` (feature `cpal-backend`) — cpal speaks PipeWire-ALSA shim, abstracts away the pipewire-rs FFI. `CpalSource` capture, `CpalSink` playback. 48 kHz / stereo / S16LE hardcoded both sides.
  - `proto::ControlMessage::StartAudioRoute{direction}` + `StopAudioRoute`. `AudioStreamInit` header on first frame of `StreamKind::Audio`.
  - Daemon-core: `AudioRegistry` per-peer, perm gates `AudioIn`/`AudioOut`. `StartAudioRoute` opens Control + provisions sink/source/streams; `audio_render_loop` drains inbound to `CpalSink`, `audio_pump_loop` drains `CpalSource` to outbound stream.
  - Companion native: `nativePollAudioControl`/`nativeSendAudioChunk`/`nativePollAudioChunk`/`nativeStopAudioStream`. `streams_accept_loop` demuxa `StreamKind::Audio` inbound.
  - Companion Kotlin: `AudioRouter` con `AudioRecord` (mic → host) + `AudioTrack` (host → device). `WireAudioControl.kt` decoder. Manifest gana `RECORD_AUDIO` + `MODIFY_AUDIO_SETTINGS` + `FOREGROUND_SERVICE_MICROPHONE`; service.foregroundServiceType += `microphone`.
  - **Pendiente nice-to-have**: notification widget MediaSession para que el usuario corte mid-stream desde la barra de notificaciones Android sin abrir la app. Funcional sin esto.
- [x] **Step 12** — `clipboard` sync con perm gates por device
  - `ansync_clipboard::WaylandClipboard` (feature `wayland`) — wrappea `wl-clipboard-rs` con `tokio::task::spawn_blocking`. Lee/escribe `text/*` + blobs MIME-tagged.
  - `StreamKind::Clipboard` tag 0x08. Mensaje `ClipboardMessage::Text|Blob` (ya existía en proto). Inbound: perm gate `ClipboardIn`. Outbound vía `DaemonAction::SyncClipboard` (gate `ClipboardOut`).
  - D-Bus `Device.SyncClipboard()` empuja el clipboard actual del host al peer. Inbound se autodispara por accept loop.
  - Companion native: `nativeSendClipboardText` (one-shot stream open + send) + `nativePollClipboardText`. Blob payloads se loguean + descartan por ahora (text-only para Step 12 ship).
  - Companion Kotlin: `ClipboardBridge` polea native + `ClipboardManager.setPrimaryClip`. `pushToHost()` lee `primaryClip` y manda via JNI. `AnsyncCompanionService` arranca/para el bridge.
  - `Capabilities::CLIPBOARD` default-on en `DaemonConfig`.
- [x] **Step 13 (BT-HID) dropped 2026-06-19** — Companion `TouchpadActivity` + `GamepadActivity` + uinput cubren input remoto. BT-HID standalone (Android-as-HID sin companion) sale del scope; `bluer` dep + `bt_hid.rs` + `InputBackend::BtHid` + `bluez` nix retirados.
- [x] **Step 14** — Nix module + crane derivation
  - `nix/package.nix` — crane build, importa workspace, instala udev rule + systemd user unit a `$out`.
  - `nix/module.nix` — NixOS module consolidado. Importa `uinput.nix` + `v4l2loopback.nix`. Opciones `services.ansync.{enable,user,package,extraGroups}`. Suma el user a `input`/`video`. Wirea systemd user unit con sandboxing (`ProtectSystem=strict`, etc.).
  - `nix/hm-module.nix` — home-manager fallback para usuarios no-NixOS. `programs.ansync.{enable,package,autoStart}`.
  - `flake.nix` expone `nixosModules.default`, `homeManagerModules.default`, `packages.default = ansyncPkg`, apps `ansyncd`/`ansyncctl`.
- [x] **Step 15** — README + docs
  - README expandido con tabla de status, arquitectura ASCII, instalación NixOS + manual, flujo de pair, surface D-Bus completa, ejemplos `gdbus call`, tabla de perms, troubleshooting, comandos de logs. No docs site separado (todo cabe en README).
- [x] **Step 16** — Pure-Rust ADB (`adb_client` 2.x)
  - Todas las `Command::new("adb")` de `crates/pairing/src/cable.rs` migradas a `ADBServer::default().get_device_by_name(serial)` + `ADBServerDevice::{reverse, reverse_remove_all, shell_command, install}`. Sync API → wrap en `tokio::task::spawn_blocking`.
  - Beneficio: cero parsing de stdout, errores estructurados via `RustADBError`. Sigue requiriendo `adbd` en el host (adb_client habla el protocolo, no USB directo).
- [x] **Step 17** — APK auto-fetch desde GitHub releases
  - `ansync_pairing::release::fetch_latest_companion()` — `reqwest` con `rustls-tls` (cero OpenSSL). Query a `api.github.com/repos/SergioRibera/ansync/releases/latest`, picks first asset `companion*.apk`.
  - Cache en `$XDG_CACHE_HOME/ansync/companion-{tag}.apk` con size + SHA-256 verify (usa `digest` field cuando GitHub lo expone; warning + skip cuando no).
  - `query_installed_version(serial, package)` parsea `dumpsys package` por `versionName=`.
  - `ansyncctl pair` ahora: si no hay `--apk` / `$ANSYNC_COMPANION_APK` / `/usr/share/ansync/companion.apk` Y el companion no está instalado → auto-fetch + install. Override `--apk` sigue funcionando para CI / nightlies.

## Retoques finales (post-roadmap)

Gaps identificados al cerrar el roadmap. Ordenados por severidad. Cada uno es bounded y aislado — buen material para sesiones cortas.

### Bloqueantes para v1 stable

- [x] **R1 — APK outdated upgrade flow (cerrar Step 17 spec)**
  - `query_installed_version` existe pero no se usa. Comparar `versionName` del device con `tag_name` del release fetched.
  - Si companion presente + outdated:
    - Default: prompt CLI `"upgrade companion {old} → {new}? (y/N)"` en `ansyncctl pair`.
    - Flag `--auto-upgrade` skip prompt + install.
    - Flag `--skip-upgrade-check` skip net call si user quiere offline.
  - Actualmente solo install-when-missing está wirado.
  - Files: `bins/ansyncctl/src/main.rs::pair`.

- [x] **R2 — Audio mid-session permission revoke**
  - `daemon-core::audio_inbound_loop` lleva `_permissions` (unused). Toggle `audio_in` off mid-stream no corta el flujo.
  - Mirror el patrón de `input_stream_loop`: re-check perm per-chunk, surface clean cuando flip to off (drop frame + return).
  - Mismo para `audio_pump_loop` re `audio_out`.
  - Files: `crates/daemon-core/src/lib.rs`.

- [x] **R4 — Notifications wire (Step 4 leftover surfaced)**
  - `proto::NotificationMessage` + `Capabilities::NOTIFICATIONS` existen pero sin `StreamKind::Notifications` ni handlers.
  - Add tag 0x09. Daemon-core accept loop demux + gate `Permission::Notifications`. Surface via D-Bus signal `Device.NotificationPosted(app, title, body)`.
  - Companion side: `NotificationListenerService` (Android) + JNI bridge similar al clipboard pattern.
  - Files: `crates/transport/src/{lib,quic}.rs`, `crates/daemon-core/src/lib.rs`, `android/src/lib.rs`, nueva `android/app/src/main/java/.../NotificationForwarder.kt`.

- [x] **R9 — Validar `nix build .#default`**
  - `nix/package.nix` escrito pero nunca ejecutado. Posibles issues: `lib.cleanSourceWith` filter incompleto, missing buildInput (ferricast path deps), `LIBCLANG_PATH` no propagado al `buildPackage` (solo está en commonArgs).
  - Correr `nix build .#default` desde `/home/s4rch/Public/rust/GamerOS/ansync`. Fix lo que rompa.
  - Validar también `nix build .#ansyncd .#ansyncctl` (apps).
  - Verificar que `nixosModules.default` no rompe `nix flake check` (sin entrar al config de un host real).

### v1 known-limitations aceptables (UX polish)

- [x] **R3 — Clipboard bidi listener-driven (sin UI)**
  - **device → host**: `ClipboardBridge.start()` registra `ClipboardManager.OnPrimaryClipChangedListener` que llama `pushToHost()` en cada cambio. `stop()` desregistra. Companion side sin gate (Android otorga todo); host decide via `ClipboardIn`/`ClipboardOut`.
  - Echo guard companion: `lastFingerprint` (`"t:<text>"` para plain, `"u:<uri>"` para image MediaStore URI) seteado antes de cada `setPrimaryClip` inbound. Listener compara antes de pushear → cero ping-pong.
  - **host → device**: `ansync_clipboard::WaylandClipboardWatcher` (feature `wayland`) bind `zwlr_data_control_manager_v1` + `data_device` para el seat default. Worker thread dedicado corre `EventQueue::blocking_dispatch`; cada `selection` / `primary_selection` event emite `()` en mpsc tokio. Daemon-core `host_clipboard_watcher` task drena el receiver, debounce 50ms, itera `MirrorRegistry.entries()`, gate per-peer `ClipboardOut`, llama `push_clipboard_to_peer`. Compositors soportados: sway/hyprland/river/KDE Plasma 6+/COSMIC/niri. GNOME (mutter) degrada a manual via `Device.SyncClipboard` con info-log explícito.
  - X11 fuera de scope v1; pattern análogo con `xfixes` queda para feature flag futuro si surge demanda.

- [x] **R5 — SAF FS mutaciones (cerrar Step 9e)**
  - `AnsyncFsServer` retorna ENOSYS para `write/create/unlink/rename/truncate/chmod`. Implementar usando `DocumentsContract.createDocument` / `deleteDocument` / `renameDocument` / `OutputStream` via `openOutputStream(uri, "w" | "wa" | "rwt")`. `chmod` deja `ENOSYS` (SAF no expone modes — limitación intencional).
  - Files: `android/app/src/main/java/.../AnsyncFsServer.kt`.

- [x] **R6 (BT-HID full profile) dropped 2026-06-19** — Step 13 retirado entero; ver entry de Step 13.

- [x] **R7 — Android MediaSession widget para audio route**
  - `AudioMediaSession.kt` envuelve `android.media.session.MediaSession` (raw API, no Compat — minSdk 26 ya cubre). `FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS` activa AVRCP / hardware media keys / Wear OS / Auto. Lock-screen widget aparece automático cuando `PlaybackState = PLAYING`.
  - `AudioFocusRequest` con `AUDIOFOCUS_GAIN` + listener: call entrante → `AUDIOFOCUS_LOSS_TRANSIENT` → pausa; vuelta → resume. `AUDIOFOCUS_LOSS` permanente → teardown via `startService(ACTION_STOP_*)`.
  - `MediaStyle` notif (LOW importance, channel `ansync.media`, NOTIFICATION_ID 5) muestra título según dirección (`Mic → PC` / `PC audio → phone` / `Two-way audio`), action(s) "Stop mic"/"Stop PC audio" en compact view. Persistent notif principal sigue mostrando streams como antes (R7 suma, no reemplaza).
  - Wired en `AnsyncCompanionService.handleStartAudio` + `handleStopAudio` + `startAudioFromTile` + `stopAudioFromTile` + `onDestroy`. Dirección merge/peel mantiene MediaSession sincronizada con `AudioRouter` actual.
  - Gradle: `androidx.media:media:1.7.0` agregado al version catalog para `androidx.media.app.NotificationCompat.MediaStyle`.

- [x] **R8 — v4l2loopback card_label per-peer** (cerrado vía dyn-ctl ioctl)
  - Re-evaluado: v4l2loopback 0.13+ expone `/dev/v4l2loopback` con `V4L2LOOPBACK_CTL_ADD` (struct `v4l2_loopback_config` con `card_label[32]` per call). Mismo path que usa `v4l2loopback-ctl add`.
  - Nuevo `ansync_camera::dyn_ctl`: raw libc::ioctl sobre el control device. `add(label, w, h) -> (nr, /dev/videoN)`, `remove(nr)`, `version()` con gate `>= 0.15` antes de trustear el struct layout. Static assert `size_of::<LoopbackConfig>() == 72` previene drift.
  - `V4l2LoopbackSink::register` ahora intenta dyn-add primero (label = `"<Build.MODEL> (Ansync)"` derivado del peer name vía U1 Hello), cae a static scan si `/dev/v4l2loopback` no existe o el ioctl falla. `unregister` REMOVE el node owned + drop del fd antes de REMOVE (kernel devuelve EBUSY si hay openers). Rollback REMOVE en path de error post-add.
  - `nix/v4l2loopback.nix` reescrito: `devices=0` (sin pre-cargar nodes), udev rule para `/dev/v4l2loopback` group `video`, catch-all rule para video[0-9]* whose driver es v4l2loopback. README troubleshooting actualizado.
  - 4 tests label encoding (build_card_label) + 2 ABI tests pasan. PipeWire-camera fallback queda como nice-to-have futuro pero ya no necesario para resolver per-peer label.

- [x] **R10 — Sensors** (dropped del scope sesión 2026-06-19; sin demanda real)

- [x] **R11 — Clipboard blob bidi**
  - Companion descarta `ClipboardMessage::Blob` silenciosamente. Wirea image MIMEs (`image/png`, `image/jpeg`) via `ClipData.newUri` + `MediaStore`. Text-only en Step 12 ship.
  - Files: `android/src/lib.rs::clipboard_in_loop`, `android/app/src/main/java/.../ClipboardBridge.kt`.

- [x] **R12 — Cleanup `audio_inbound_loop` permissions param**
  - `_permissions: Arc<dyn PermissionsStore>` es noise. O lo usa (R2) o sale del signature.
  - Resuelto automáticamente cuando R2 cierra.

## Estabilización (sesión 2026-06-17/18)

Surfaceado mientras se probaba el pair WiFi + mirror lifecycle real con DMS. Cada item commit single-line, todos mergeados. Bloque dejado acá para no contaminar el roadmap principal con bugfix detail.

### Cerrado

- [x] **S1 — Clipboard MIME priority** (`5322502`)
  - `wayland::read` antes pedía `PasteMime::Text` → Firefox/Krita devolvía `text/html` con `<img>` base64 cuando había imagen → Android leía HTML en vez de bitmap.
  - Nuevo `pick_best_mime`: enumerate `get_mime_types`, prioriza `image/png|jpeg|webp|gif|bmp|tiff`, después cualquier `image/*`, después `text/plain` (variantes UTF-8), por último cualquier otro. 4 tests.

- [x] **S2 — Mirror window: subprocess per ventana** (`07875a1` → `82a3012` → `e64e0b7` → `0169db2` → `c0d647e`)
  - Iteramos varias arquitecturas. Final: `ansyncd mirror-renderer` subcommand spawnea proceso hijo por ShowScreen. Cada child = su propio winit EventLoop → resuelve la limitación `EVENT_LOOP_CREATED` que prohíbe rebuilds in-process. IPC stdin/stdout pipes (drop Unix socket: sin filesystem, cleanup automático). `ansync_video::ipc` postcard length-prefixed (`HostMsg::{Config, EncodedChunk, Shutdown}` + `RendererMsg::Input`).
  - Daemon: video_stream_loop ya no decoda, solo fan-out de chunks a `entry.video_tx` (None = drop silencioso).
  - Lifecycle limpio: D-Bus ShowScreen spawn child, close X → child exit → on_exit limpia slots + emit HideScreen → companion StopScreenCapture. Daemon arranca sin ventana. Sin auto-spawn desde `StreamKind::Video` (solo D-Bus puede abrir).

- [x] **S3 — Pair WiFi: filter unreachable mDNS candidates** (`9fa2675`)
  - `browse_pair_candidates` antes pickaba `info.get_addresses().iter().next()` (HashSet order indeterminado). A veces fe80::link-local sin scope id → `connect` EINVAL.
  - Fix: filtra fe80::/10 + 169.254/16 + loopback. Rankea IPv4 > IPv6. Dedupe por pubkey: nuevo entry solo overrides si rank mejor.

- [x] **S4 — Version pinning + skip-install** (`9ad1aa5`)
  - `expected_version()` lee `option_env!("ANSYNC_RELEASE_VERSION")` con fallback `CARGO_PKG_VERSION`. CI exporta el git tag → todos los crates concuerdan sin tocar Cargo.toml (cache intact).
  - `fetch_companion(tag)` GET `/releases/tags/{tag}`. `fetch_latest_companion` delega a `fetch_companion(expected_version())`. Tolera tag con y sin `v` prefix.
  - `ansyncctl pair`: query installed `versionName`, si matchea `expected_version_bare()` → skip install + skip fetch. Si distinto → fetch matching tag + install. `--auto-upgrade` ahora no-op (compat). `--skip-upgrade-check` mantiene escape offline.

- [x] **S5 — Pair broadcast delivery** (`b7edcaa`)
  - Companion headless (post-U4a) nunca se lanza por user → Android lo deja en "stopped state" post-install → broadcasts dropped silently sin `FLAG_INCLUDE_STOPPED_PACKAGES`.
  - Fix: `am broadcast --include-stopped-packages …` + idempotente `pm grant POST_NOTIFICATIONS` post-install y pre-broadcast (cubre skip-install-on-version-match path).

- [x] **S6 — Raw adbd protocol para reverse** (`e1ed6c9`)
  - User: "no shellear adb CLI, usar crate". `adb_client::ADBServerDevice::reverse()` lee solo el primer OKAY; `reverse:forward` requiere DOS (server ack + adbd ack post-bind). Sin el segundo → host cierra TCP antes que adbd termine de instalar el listener → companion `connect` ETIMEDOUT.
  - Fix: hablamos directo a `127.0.0.1:5037` por TCP. `open_adbd / adb_send_cmd / adb_read_status` helpers. Cero `Command::new("adb")` en el proyecto.

- [x] **S7 — Auto-reconnect via livenessProbe** (`db3c818` + `9209600`)
  - `HostDialer.connected` antes solo flippeaba a false en eventos de red. Daemon restart sin link drop → companion ciego → no redial.
  - Native: `static CONNECTED: AtomicBool` set true post-handshake, `Drop` guard en `streams_accept_loop` la apaga. Nuevo JNI `nativeIsConnected`.
  - Kotlin: `livenessProbe` Runnable re-post cada 3s. Si `connected && !native.isConnected` → flip + dialOnce. `try/catch Throwable` por si APK viejo no tiene el símbolo (evita crash del service).
  - Daemon `handle_connection` dedupe: al entrar, `.close()` cualquier conn previo para el mismo peer (Arc::ptr_eq). Al salir, solo emite Disconnected si `mirror_entry.conn` SIGUE siendo el nuestro — evita que un conn evicted flap el estado del nuevo.

- [x] **S8 — Companion keep-alive (doze + OOM)** (`9631338`)
  - `KeepAlive.kt`: `WifiManager.WifiLock` mode `WIFI_MODE_FULL_LOW_LATENCY` (API 29+, fallback `FULL_HIGH_PERF`). Acquired in `Service.onCreate`, released in `onDestroy`. Mantiene Wi-Fi radio en full-power → keep-alive QUIC sobrevive screen-off.
  - Nuevo `SetupStep.BatteryWhitelist`: lanza `ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS` con `Uri.fromParts("package", …)`. Fallback a `ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`. `KeepAlive.isBatteryWhitelisted(ctx)` para `isDone`.
  - Manifest: nueva perm `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`.

- [x] **S12 — PipeWire mic share end-to-end (sesión 2026-06-30)**
  - Síntoma reportado: el mic de Android se escuchaba por los audífonos del host (mal routing) y el virtual mic registrado no producía sonido / sonaba fatal con ruido constante.
  - Diagnóstico vía `pw-link -l` + diag PCM stats (mean_abs / peak_abs por ventana de 50 packets en `audio_render_loop`):
    1. `audio_inbound_loop` exitía con "sink missing" porque el QSTile-driven mic share del companion abre `StreamKind::Audio` directo sin `StartAudioRoute` previo por D-Bus → no había sink provisioned.
    2. Plan A inicial publicaba el nodo como `Audio/Source/Virtual` → adapter solo exponía `capture_FL/FR` outputs, sin `playback_FL/FR` inputs → feeder caía al default sink (easyeffects → speakers).
    3. PCM decodificado era correcto (mean variando con voz) pero ruido brutal — `VecDeque<Bytes>` ring scrambleaba orden temporal al re-encolar leftover chunk con `push_back`.
    4. Aún tras `push_front` fix seguía buzzy — quantum PipeWire (1024) ≠ Opus frame (960) → straddling de packets en cada callback + silence-pad mid-buffer cada underrun.
  - Fixes:
    - `daemon-core::audio_inbound_loop`: lazy-provisiona sink desde el `AudioStreamInit` header si `entry.sink` está vacío (perm gate `AudioIn` re-checkeado). Plumbing `audio_backend: SharedAudioBackend` por `AcceptCtx → accept_loop → handle_connection`.
    - `pipewire_backend::run_virtual_sink`: Plan A — `core.create_object::<Node>("adapter", { factory.name=support.null-audio-sink, media.class=Audio/Sink, audio.position=FL,FR, ... })` con roundtrip `core.sync(0)` antes de conectar feeder. `Audio/Sink` (no `Audio/Source/Virtual`) garantiza input ports + auto monitor source.
    - `pipewire_backend::ByteRing`: reemplazo del `PcmRing` chunk-based por buffer byte-level continuo (`VecDeque<u8>`, cap 512 KiB). Process callback drena `slot.len()` exacto, cero straddling. Prebuffer 40 ms (2 frames Opus) gate la primera salida y se re-arma en underrun → emite `chunk.size=0` (silencio limpio, no click).
    - Feeder stream gana `node.latency = "960/48000"` → PipeWire negocia quantum 960, alineado a packet Opus.
    - Diag stats removidas tras validar fix.
  - Resultado: mic share funciona excelente, sin ruido, sin glitches. PipeWire surface `<peer_name> (Ansync)` aparece en Discord / Firefox / etc.

- [x] **S11 — Camera UX + sink fixes**
  - **Short writes**: `V4l2LoopbackSink::write_frame` ahora loop hasta drenar `frame.len()`. POSIX write(2) puede devolver < total (v4l2loopback cappea por buffer slot del ringbuffer). Retry en `EINTR`/`EAGAIN`; surface `WriteZero` si kernel devuelve 0 bytes (sin consumer).
  - **StopCamera no propagaba al device**: `handle_stop_camera` ahora dispara `ControlMessage::StopCamera` por `StreamKind::Control` ANTES del teardown local. Sin esto, `CameraSession` Camera2 + MediaCodec quedaban corriendo (no hay backpressure por Surface input) → LED + drain de batería persistentes. Mantenemos teardown local aunque el send falle (peer puede haber colgado).
  - **DMS picker popup**: nuevo `popoutState === "camera"` en `AnsyncWidget.qml`. Click en camera tile OFF → popup con DankDropdowns para lens (back/front), resolución (1920x1080 / 1280x720 / 4K / etc.), fps (15/24/30/60), codec (h264/h265), aspect (crop/letterbox/stretch), bitrate (DankTextField), toggle stabilization. Pre-seeded con `pluginData` defaults. Click ON → stop directo (sin popup). Defaults siguen modificables desde Settings.

- [x] **S10 — NixOS module abre firewall por default**
  - Síntoma reportado: tras restart del daemon el companion mostraba `companion reachable on LAN` repetido pero cero handshake. Diagnóstico: `networking.firewall.allowedUDPPorts` en el host solo tenía 5353; UDP 47215 (QUIC) bloqueado por kernel → `ConnState::Disconnected` permanente.
  - `nix/module.nix` gana opciones `services.ansync.quicPort` (default 47215) + `services.ansync.openFirewall` (default true). Cuando ambos están on, `networking.firewall.allowedUDPPorts = [ quicPort 5353 ]` se setea automático.
  - README: nueva sección Firewall (post Install manual) + entry de Troubleshooting para device pegado en Disconnected con mDNS funcionando.
  - Eval verificada: `nix eval ... config.networking.firewall.allowedUDPPorts → [ 5353 47215 ]`.

- [x] **S9 — DMS plugin: toggle buttons + auto-detect pair**
  - Cambios en `~/.config/DankMaterialShell/plugins/Ansync/` (no es repo git, sin commit).
  - `AnsyncService.qml`: `streams: { id: {mirror, mic, camera, audio} }` cache local + helpers `isStreamOn / toggleScreen / toggleMic / toggleCamera / toggleAudio`. Reset cache on device-offline. Nuevas funciones `browseAvailable / startWifiPair / submitPin / cancelPair` para el flujo D-Bus de PairingSession. Signals `wifiCandidatesFound / wifiPairStarted / wifiPairAwaitingPin / wifiPairCompleted / wifiPairFailed`. `_busMonitor` ahora parsea PairingSession `PropertiesChanged` + `Completed` + `Failed`.
  - `AnsyncWidget.qml`: per-device action grid colapsada de start/stop pairs → toggle único cada uno (mirror, mic, camera, audio) que flippea icon + label + color según `isStreamOn`. Header pair antes era dos botones (USB / WiFi); ahora UNO que dispatchea: `listAdbDevices()` → serials > 0 → state=pair, else → state=wifiPair + browse. PIN modal con `DankTextField` + Submit (valida 6 dígitos antes de habilitar).
  - Buttons mirror/mic/camera/audio ya no usan `enabled: device.live` — daemon es idempotente y el grey-out confundía al user ("did you remove them?").

### Pendiente para próxima sesión

- [x] **N1 — DMS plugin polish** (cerrado sesión 2026-06-19)

- [x] **N2 — OEM-specific autostart hints** (cerrado sesión 2026-06-19)

- [x] **N3 — WakeLock partial opcional para sessions activas** (cerrado 2026-06-19)
  - `KeepAlive` ahora wrappea `PowerManager.PARTIAL_WAKE_LOCK` además del `WifiLock`. Refcounted: `streamStarted()` / `streamStopped()` lo acquire/release cuando el counter cruza 0→1 / 1→0. `refreshCpuLockPolicy()` cubre flips mid-session (acquire si user opt-in con streams ya activos, release si revoca).
  - Gate `PREF_CPU_WAKE_LOCK` (default off). Flippable via `adb shell am broadcast -a org.gameros.ansync.action.SET_CPU_WAKE_LOCK --ez enabled true` — el receiver persiste el bool + dispara `refreshCpuLockPolicy()`. DMS plugin / futura settings activity / tile pueden dispatchear el mismo broadcast.
  - `AnsyncCompanionService` mantiene set `activeStreams: Set<String>` (`"capture"`, `"camera"`, `"audio"`) + helper `markStream(key, active)` idempotente. Wireado en handleStartCapture/stopCapture, handleStartCamera/handleStopCamera, handleStartAudio/handleStopAudio, startAudioFromTile/stopAudioFromTile.
  - Battery cost real ~5%/h cuando on. Sin pref off → comportamiento idéntico al anterior.

- [x] **N5 — MediaSession widget activo durante mirror** (cerrado 2026-06-19)
  - Nuevo `MirrorMediaSession.kt`: `MediaSession` con `FLAG_HANDLES_MEDIA_BUTTONS | FLAG_HANDLES_TRANSPORT_CONTROLS`, `PlaybackState.STATE_PLAYING`, action única "Stop" que funnels via `ACTION_STOP_CAPTURE` (mismo path que QSTile / persistent notif). Headset pause key collapsed a stop (MediaProjection sin primitive pause).
  - Channel `ansync.media.mirror` LOW importance separado del audio channel. `NOTIFICATION_ID 6`. Title = "Mirroring to <hostLabel>" (host name desde `PREF_HOST_NAME`).
  - `AnsyncCompanionService`: nuevo field `mirrorMediaSession`. `startMirrorMediaSession()` idempotente, llamado en post-capture-start handler. `stopCapture()` y `onDestroy` lo release/null. MediaProjection.Callback.onStop ya pasa por `stopCapture` → no doble teardown.
  - Aditivo a persistent notif principal (R7 audio + mirror = dos MediaSessions concurrent; lock screen muestra ambas swipeable).
  - R7 ya cubre audio. Sumar widget similar para "mirror activo" — corte directo desde lock screen sin abrir notif shade.

- [x] **N6 — D-Bus signal para stream state changes** (cerrado 2026-06-19)
  - `Device.StreamStateChanged(kind, active)` ya existía + daemon-initiated paths emitían. Gap: companion-side teardown (tile off / MediaCodec crash / projection revoke / mic perm revoke) no fan-out al signal.
  - `camera_stream_loop` + `audio_inbound_loop` ganan exit-guard. Si al salir el slot relevante (`frame_tx` para camera, `inbound_tile_kind` para audio) sigue Some → companion stop unilateral → guard limpia slot + emite `StreamStateChanged(kind, false)`. Daemon-initiated path limpia el slot ANTES del stream close → guard no-op (sin emisión doble).
  - `AudioEntry` gana `inbound_tile_kind: StdMutex<Option<&'static str>>`. `handle_start_audio` recibe el tile name ("mic" para StartMicrophone, "audio" para StartAudioRoute) y lo persiste cuando `need_in`. `handle_stop_audio` lo clearea.
  - Video stream loop ya tenía `InboundGuard` que disparaba `HideScreen` action → emit "screen=false" cubierto desde antes.

- [x] **N7 — Cable pair runtime perm auto-grant** (cerrado 2026-06-19)
  - `adb_client::install` no expone `-g`. Workaround sin PR upstream + sin shell-out al binario `adb`: loop sobre array estático `COMPANION_RUNTIME_PERMS` corriendo `device.shell_command(["pm", "grant", pkg, perm])` por cada permiso "dangerous" declarado en el manifest. Idempotente (re-grant si el user revoca mid-session, cubre tanto install path como skip-install-on-version-match path).
  - Array hoy: `POST_NOTIFICATIONS`, `CAMERA`, `RECORD_AUDIO`. Normal install-time perms (INTERNET, WAKE_LOCK, FOREGROUND_SERVICE_*) NO se incluyen — `pm grant` les devuelve error. AppOps perms (SYSTEM_ALERT_WINDOW, USE_FULL_SCREEN_INTENT, REQUEST_IGNORE_BATTERY_OPTIMIZATIONS) tampoco — los maneja el `SetupNotif` walkthrough del companion.
  - Maintenance rule en `CLAUDE.md` § Reglas duras: cada vez que el manifest gane un runtime perm nuevo, sumar el nombre al array. Lock-in del check.

- [ ] **N8 — Multi-host companion**
  - Companion guarda UN solo `PREF_HOST_PUBKEY_HEX` + `PREF_HOST_NAME`. User explícitamente deferreó ("nah, estamos bien por ahora").
  - Cuando esto se requiera: refactor a `Set<HostEntry>` en SharedPreferences (JSON). HostDialer itera todos. UI para gestionar (revocar host, switch primary). ShareActivity gana picker.

- [x] **Share (Quick Share-style) — files + URLs bidi** (cerrado 2026-06-19)
  - Proto: `Message::Url(UrlMessage { url })`, `StreamKind::Url` (tag 0x0b), `Permission::ShareReceive` (default on), `Capabilities::SHARE`.
  - Host (Linux):
    - D-Bus: `Device.SendFiles(paths: as) -> u`, `Device.SendUrl(url: s)`, signal `Device.FileReceived(path: s)`.
    - daemon-core: `DaemonAction::{SendFiles,SendUrl}`. `handle_send_files` itera paths + abre `StreamKind::Files` por archivo + reusa `send_file`. `handle_send_url` postcard one-shot. `url_inbound_loop` gated por `Permission::ShareReceive`, ejecuta `xdg-open` sync (paired = trusted; threat model documentado en proto). `files_stream_loop` post-receive emite `FileReceived` + `notify-send` (shell out, sin libnotify dep).
    - ansyncctl: `push <id> <paths...>` (variadic) y `url <id> <url>` ahora pasan SIEMPRE por D-Bus. QUIC dial directo + `MdnsDiscovery` runtime para `push` eliminados.
  - Companion (Android):
    - JNI: `nativeSendFile(path)`, `nativeSendUrl(url)`, `nativePollIncomingUrl()`, `nativePollReceivedFile()`. `streams_accept_loop` acepta `StreamKind::Url` → `url_in_loop` → mpsc → Kotlin worker. Files inbound stream loop pushea `path` post-completion al `received_files_rx`.
    - Kotlin: `ShareActivity` (translucent) registra intent-filters `ACTION_SEND text/plain + */*` + `ACTION_SEND_MULTIPLE`. URL detect via `Patterns.WEB_URL`; arbitrary files copiados a `cacheDir/share/` (Rust side toma path filesystem). Multi-file via worker thread + per-file ok counter. `ShareTile` QSTile lanza `ShareActivity` empty → `ACTION_GET_CONTENT` picker. Receive workers en `AnsyncCompanionService` polean URL → notif "Open link from host?" + tap → `ACTION_VIEW`. File received → `MediaScannerConnection.scanFile` + notif "tap to open" via `Uri.fromFile`.
  - Capabilities default-on en host + companion. Tile + intent-filters + receive notifs todos shipping. Multi-host picker queda gated por N8.

- [ ] **N9 — Nautilus / file-manager "Send to" extension**
  - PC side: Python extension (`Nautilus.MenuProvider`) que lista paired+online devices vía D-Bus `Manager.ListDevices` filtrado por `Device.State == Active`. Tap → `Device.SendFiles([selection])`. KDE Dolphin servicemenu equivalente (`*.desktop` en `~/.local/share/kio/servicemenus/`).
  - Empaquetado: instalado vía `nix/package.nix` postInstall a `$out/share/nautilus-python/extensions/`. Opt-in con `services.ansync.installFileManagerExtensions = true`.
  - No bloqueante para v1 — share funciona end-to-end vía `ansyncctl push` + Android share-sheet. Sumar cuando primer usuario lo pida.

### Notas para retomar

- Empezar por R1, R2, R4, R9 (bloqueantes). Cada uno cierra en <1h.
- R5 + R6 son medias jornadas individuales por la complejidad del wire format (SAF mutations) / BT stack.
- R7, R11 son cosmética: dejan para post-v1.
- ~~R8 documentar como upstream limitation en README + cerrar como WONTFIX.~~ Cerrado vía dyn-ctl ioctl (sesión 2026-06-18).
- ~~R10 evaluar demanda: si nadie lo pide, dropear del scope.~~ Dropped sesión 2026-06-19.

## Triaje UX post-v1 (sesión 2026-06-15)

Surfaceado tras primera ronda de smoke test real con DMS widget + companion. PLAN.md es el roadmap canónico — esta sección persiste el triaje para continuar en sesión nueva.

### Síntomas reportados

1. **Plugin DMS roto** — syntax errors QML, multi-screen no muestra data en pantallas secundarias, `Theme.errorText` undefined, `parent.flag` undefined dentro de `Connections{}`, deprecated `checked` injection, `anchors.fill` dentro de Column.
2. **Pair no completa** — ni USB ni WiFi. Sin logs aún para diagnosticar.
3. **Sin estado de conexión visible** — D-Bus expone `State: string` pero no hay señal de cambio ni state machine explícita (Online / Idle / Disconnected / Pairing).
4. **Hostname / device name no se intercambia** — daemon usa `device_id` (uuid). Falta `gethostname()` host-side + `Build.MODEL + " " + Build.MANUFACTURER` companion-side.
5. **Companion UI tosca** — Activity con botones (Start screen capture, Open touchpad, picker SAF, paired host display) rompe la analogía scrcpy: control debe vivir en host.

### Fixes ya aplicados (esta sesión)

- [x] Plugin DMS QML balance + multi-screen: `pragma Singleton` en `AnsyncService.qml`, `qmldir` con `singleton AnsyncService 1.0 AnsyncService.qml`, widget consume `import "."` + `readonly property var svc: AnsyncService`. `pluginData` propagado vía `Component.onCompleted` + `Connections{ target: root }`.
- [x] `Theme.errorText` → `"white"` (no existe en DMS Theme).
- [x] Permission row: `id: permRow` + helper `refresh()` + `(value) => ...` formal param, drop `parent.flag` desde dentro de `Connections{}`.
- [x] `anchors.fill: parent` dentro de Column → `width: parent.width` (pair Column + forget Column).
- [x] Pill wrappers `Item { implicitHeight: parent.widgetThickness }` → bare `Row{ spacing }` / `Column{ spacing }` (matcha DankFerricast).

### Pendientes post-v1 (orden propuesto)

- [x] **U1 — Hostname / Hello frame** (cierra síntoma 4)
  - `StreamKind::Hello` (tag 0x0a) one-shot bidi. Primer y único frame post-handshake en ambas direcciones es `Envelope{Message::Hello{device_id, name, capabilities}}`.
  - Host: `libc::gethostname` con fallback `$HOSTNAME`.
  - Companion: `Build.MANUFACTURER + " " + Build.MODEL` cacheado vía `nativeSetDeviceName` (llamado en Service.onCreate + MainActivity + PairingReceiver).
  - Daemon `hello_inbound_loop` actualiza `StoredPeer.name` cuando cambia. Companion `hello_in_loop` stashea host name en `last_host_name`; `nativePollHostName` + MainActivity LaunchedEffect persisten en `PREF_HOST_NAME`.
  - D-Bus `Device.Name` ya devolvía `peer.name`; ahora refleja hostname real automáticamente.

- [x] **U2 — Connectivity state machine + D-Bus signal** (cierra síntoma 3)
  - `ConnState{Disconnected, Pairing, Authenticated, Active}` en `ansync_dbus::state`. Registry `Arc<StdMutex<HashMap<DeviceId, ConnState>>>` en `DaemonState`.
  - `Device.State` ahora lee del registry. `Device::emit_state_changed` helper emite el auto-generated `PropertiesChanged` (state) + custom signal `Manager.DeviceConnectivityChanged(id, state)`.
  - Transiciones en `handle_connection`: Authenticated cuando arranca (post-TLS), Active cuando `send_hello` ok, Disconnected en cleanup. `Pairing` reservado para ansyncctl pair flow (no wire en daemon-core).
  - DMS widget pinta verde/amarillo/gris suscribiéndose a `Manager.DeviceConnectivityChanged` (single fan-out path) o `PropertiesChanged` per device.

- [x] **U3 — Diagnóstico pair fail** (cierra síntoma 2)
  - Diagnóstico end-to-end con `journalctl --user _COMM=ansyncd` + `adb logcat -s ansync.*`. Tres bugs cazados:
    1. **`adb_client::ADBServerDevice::reverse` no instala el listener en adbd del device.** `adb reverse --list` lo muestra host-side pero `/proc/net/tcp` del device nunca abre el puerto → companion `connect("127.0.0.1", port)` ETIMEDOUT. Fix: `pair_host_via_adb` shell-out al binario `adb` para `reverse` + `reverse --remove-all`. (Step 16 removió *parsing* de stdout, no usage del binario; reverse no parsea nada).
    2. **`bootstrap_host` no flushea/cerraba el TCP antes de dropear.** Tokio `TcpStream::drop` race con kernel adb-USB forwarder → companion lee "early eof" antes que el Ack atraviese el cable. Fix: `stream.flush().await + stream.shutdown().await` después de `write_envelope(Ack)`.
    3. **`adb_client::shell_command` con shell_v2 entrega framing bytes mezclados con stdout.** `companion_installed` strict-line match fallaba pese a APK presente; `query_installed_version` igual con `strip_prefix("versionName=")`. Fix: substring match (`stdout.contains("package:...")` + `find("versionName=")` con extracción hasta whitespace).
    4. **`PairingReceiver` usaba el extra `name` del broadcast como propio.** El extra trae el HOST_NAME (para display), no Build.MODEL. Resultado: peer.name en host quedaba `ansync-host` (auto-corregido en siguiente connect vía U1 Hello frame, pero feo). Fix Kotlin: `companionName = "${Build.MANUFACTURER} ${Build.MODEL}"`.
  - Verificado: pair end-to-end OK, `PeerStore` persiste, `Manager.RefreshPeers` registra path D-Bus, companion log `cable pairing complete with host ansync-host`.

- [x] **U4 — Headless companion + popups + QSTiles** (cierra síntoma 5)
  - Companion = service de fondo puro. Sin launcher icon.
  - **U4a [x]** — Transform headless:
    - Drop `MainActivity` + Compose Material3 status surface.
    - New translucent shims: `PermissionsBootstrapActivity` (walks POST_NOTIFICATIONS / RECORD_AUDIO / SAF tree picker / Accessibility settings / Notification Listener settings con toasts), `GrantScreenCaptureActivity` (MediaProjection picker), `GrantStorageActivity` (re-pick SAF tree on demand).
    - `AnsyncCompanionService.onCreate` lanza `PermissionsBootstrapActivity` si `PREF_GRANTS_BOOTSTRAPPED` off (idempotente).
    - `PairingReceiver` arranca el service post-bootstrap atomic (sin requerir abrir app).
    - Service `requestCaptureFromUser()` postea high-priority notif que abre `GrantScreenCaptureActivity` cuando host pide ShowScreen sin token activo.
    - `foregroundServiceType=dataSync|mediaProjection|camera|microphone`; service inicia en `dataSync`, promueve a tipo específico cuando capture/audio/camera arrancan (Android 14+ rechaza media-tipos sin privileged token activo).
    - Manifest: drop MAIN/LAUNCHER intent-filter; translucent activities `noHistory + excludeFromRecents`.
    - `Prefs.kt` central para `PREFS / PREF_TREE_URI / PREF_HOST_ADDR / PREF_GRANTS_BOOTSTRAPPED`.
  - **U4b [x]** — QSTiles: `MirrorTile`, `TouchpadTile`, `MicShareTile`, `AudioSinkTile`. Cada uno = `TileService` que envía Intent a `AnsyncCompanionService`. State persistido en SharedPreferences (`PREF_MIRROR_ACTIVE` / `PREF_MIC_ACTIVE` / `PREF_AUDIO_OUT_ACTIVE`). Mirror y Touchpad usan `startActivityAndCollapse` con PendingIntent (API 34+ signature).
  - **U4c [x]** — `BootReceiver` (BOOT_COMPLETED + LOCKED_BOOT_COMPLETED + MY_PACKAGE_REPLACED) + `HostDialer` con `ConnectivityManager.NetworkCallback` (Wi-Fi / Ethernet) + `HostDiscovery` mDNS reuse + exponential backoff (1s→60s). Auto-redial cuando wifi reconecta sin user action.
  - **U4d [x]** — Notif persistente recompone state-driven: por cada stream activo (mirror / mic / PC audio / camera) muestra una action button "Stop X" con PendingIntent a la action correspondiente del service. `refreshNotification()` se llama desde cada start/stop helper. Absorbe R7 — sin MediaSession completo pero functional para corte directo desde shade.
  - **U4e [x]** — WiFi pair PIN flow, headless. Cero UI nueva, cero QSTile, cero flag en `ansyncctl`. PC corre `ansyncctl pair` (sin args), si no hay ADB browse mDNS `_ansync-pair._tcp` → 1 match auto-pick, varios prompt → dial → prompt PIN. Android muestra heads-up notif "X wants to pair — PIN 123456" cuando llega `BootstrapHello`. Wire: proto `PairingMessage::PinConfirm{mac:[u8;32]}` (replaces `PinChallenge`/`PinResponse`); SHA-256 domain-sep MAC en `ansync_crypto::pair_pin_confirm`. `crates/pairing/src/wifi.rs` split en `read_pair_hello` + `respond_pair_pin` para inyectar notif entre fases. Always-on listener vive dentro de `AnsyncCompanionService` via `WifiPairManager.kt`: arranca native `nativeWifiPairListenerStart`, NSD register `_ansync-pair._tcp` con TXT `id`/`name`, worker thread polea `nativePollPairEvent` (REQUEST/BAD/LOCK/OK). 3-strike lockout per PIN. mDNS-advertised pubkey verificado contra el que cruza el wire (anti-impersonation). `--remote-addr ip:port` fallback bypass mDNS para captive/AP-isolated networks.
  - **U4e+ [x]** — D-Bus pair surface para que el widget DMS (u otro UI) dispare el flujo sin pasar por terminal. `Manager.BrowseAvailable(seconds)` → `Vec<(name, addr, pubkey_hex)>` (wraps `browse_pair_candidates`). `Manager.StartPairing(addr, expected_pubkey_hex)` → `ObjectPath` de una nueva `org.gameros.Ansync1.PairingSession` registrada en `/org/gameros/Ansync1/Pair/{uuid}`. Properties: `State` (dialing|awaiting_pin|verifying|ok|failed), `HostName`, `HostPubkeyHex`, `Address`, `Error`. Methods: `SubmitPin(pin)` (acepta dígitos con cualquier separador), `Cancel()`. Signals: `Completed(device_id, name)` + `Failed(reason)`. Worker hace dial → Hello → Ack → flip a awaiting_pin (emit `PropertiesChanged`) → espera `SubmitPin` con timeout 5min → MAC exchange → persist + `register_device` auto + emit `Completed`. Session linger 60s post-terminal antes de auto-remove. Wire identicál al CLI path (mismo `pair_pin_confirm` + envelope shape).
  - USB pair: cable = trust window, auto-accept sin tap (ya está).

- [x] **U5 — `RequestScreenCapture` wire + auto-connect** (cierra síntoma 5 + 4)
  - `ControlMessage::{RequestScreenCapture, StopScreenCapture}` nuevos en proto.
  - Daemon `action_loop::ShowScreen` ahora ALSO abre Control + manda `RequestScreenCapture` post-window-spawn. `HideScreen` simétrico (manda `StopScreenCapture`).
  - Companion native: `control_recv_loop` decoda los dos tags, push a `capture_ctrl_rx` (Vec<u8> single-byte). JNI `nativePollCaptureControl()` blocking.
  - Companion Kotlin: `AnsyncCompanionService.startCaptureControlPoller()` worker thread `ansync-cap-ctrl`. Tag 0 → `requestCaptureFromUser()` (high-priority notif "tap to grant" → `GrantScreenCaptureActivity` → MediaProjection picker → `ACTION_START_CAPTURE`). Tag 1 → `stopCapture()`.
  - Auto-connect: `HostDialer` (U4c) cubre el escenario "device unlocks → wifi up → companion dials host automáticamente". Host-side mDNS host-discovers-companion deferred (companion already announces via daemon's mDNS announce; host browse mechanism = follow-up).

- [x] **U5 — Auto-connect mDNS host-side** (cierra síntoma 5)
  - Topología real: companion = QUIC client (dials host), host = QUIC server. Companion ya auto-dial via `HostDialer` con `ConnectivityManager.NetworkCallback` + exponential backoff (Steps post-9.5 + U4c). "Host-side auto-connect" se materializa como **presence-watcher**, no como dial: daemon-core `companion_watcher` task corre `ansync_pairing::watch_pair_candidates()` (mdns-sd long-lived browse de `_ansync-pair._tcp.local.`). Cada `Resolved` cruzado contra `PeerStore` → si pubkey matchea, persiste `(DeviceId, SocketAddr)` en `DaemonState.reachable` + emite `Manager.DeviceReachable(id, addr)`. `Removed` → clear + `Manager.DeviceUnreachable(id)`. Snapshot accesible via `Manager.ReachableDevices() → a(ss)`. Widget pinta presence dot antes de que QUIC handshake complete (estado "active") — semáforo gris/amarillo/verde queda con tres datos: ConnState (handshake), reachable (mDNS visibility), Hello fresh (caps known).
  - Companion HostDialer ya cubre re-connect en suspend/resume vía NetworkCallback onAvailable; netlink-watch host-side innecesario porque el companion siempre es el iniciador del QUIC.
  - "Connect to X (IP)" botón companion ya estaba dropeado en U4a (headless). Verified.

### Tradeoffs

- U4 mata UI Activity de Steps 7c-9.5 (CameraSession Intent flow, TouchpadActivity, paired host card). SAF picker NO se puede evitar (Android requiere user grant explícito) — minimizar a primer mount.
- U3 sin logs es shot-in-the-dark. Bloqueado hasta que usuario pegue output.
- U2 toca surface D-Bus pública; coordinar con bump de versión interfaz.
- U1 trivial, base para todos los demás (DMS pinta nombre real, no `a1b2c3d4`).

### Orden recomendado

1. **U1** (hostname) — trivial, prereq cosmético para todo lo demás.
2. **U2** (state machine + signal) — base para que DMS muestre algo útil.
3. **U3** (pair fail diag) — bloqueado en logs del usuario.
4. **U4** (strip Activity) — gran refactor companion, mejor con U1+U2 ya merged.
5. **U5** (auto-connect mDNS) — última capa, depende de U2 para señalizar state.

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
- **input**: input-linux (consumido en Step 7)
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

## Touchpad Mac-style — cerrado (sesión 2026-06-24)

`TouchpadActivity` ahora delega tap/scroll/pinch/gestures a libinput. Diagnosticado con `libinput debug-events --verbose --device /dev/input/eventNN` (NN cambia por reinstall — buscar en `/proc/bus/input/devices`).

### Bugs encontrados + fixes

- **Palm rejection 100% false positive** (`cbcc0c7`). Log inicial mostraba `palm: touch N (TOUCH_BEGIN), palm detected (pressure)` para TODOS los touches.
  - Causa: Android `MotionEvent.getPressure()` devuelve ~1.0 para capacitive normal, escalábamos a 255 (max del axis), libinput hardcoded palm threshold = 130 para touchpads "unknown" → cada touch caía en palm zone → descartado.
  - Fix: nueva `scaleTouchpadPressure(raw)` en `TouchpadActivity.kt` mapea Android 0..1.5 → 30..120 (arriba del touch threshold 30, abajo del palm 130). Aplicado en `emitTouchpadSlot` + `emitTouchpadSlotHistorical`. Touchscreen raw mode + Stylus NO tocados (devices distintos con thresholds propios).
- **Jump filter al touchdown** (`2b8d03c`). 5 `kernel bug: Touch jump detected and discarded` al primer touch antes del rate-limit (24h).
  - Causa: libinput retiene "last position" interno per-slot aún después de tracking_id transition. Primer POSITION del nuevo touch saltaba >7mm del prev → discardeado.
  - Fix: bajar `TOUCHPAD_RES` 500→200 units/mm (touchpad reportado 65mm→163mm, Magic Trackpad-ish, cursor más rápido sin compositor accel) + intra-touch delta clamp 5mm vs `last_pos[slot]` HashMap. **Touchdown va RAW** (sin clamp) para que libinput tome ese POSITION como anchor del nuevo touch sin POINTER_MOTION espurio (arrastre desde último touch).
  - TouchMajor/Minor recalc a `8 * TOUCHPAD_RES` = 1600 (era 4000 hardcoded para res 500).

### Telemetría / clasificación de errores (sesión 2026-06-24)

- **Sidecar `stats_telemetry_loop`** (`f1ee08c`) per-peer en daemon (`crates/daemon-core/src/lib.rs`) + companion (`android/src/lib.rs`). Cada 5s loguea `rtt_ms / sent / lost / loss_pct / cwnd / black_holes` desde `QuicConnection::stats()` (accessor agregado en `crates/transport/src/quic.rs`). Gate `RUST_LOG=ansync_daemon_core=debug,ansync_companion_native=debug`. Auto-cancelado: daemon via `JoinHandle::abort()` al fin de `handle_connection`; companion via `Weak<QuicConnection>::upgrade()` (Arc count==0 ⇒ session muerta).
- **Counter `input_rx/tx`** (`d3e4ec6`) `AtomicU64` per-peer en daemon + global en companion. Cross-check perfecto en sesión real (sent ↔ rx 1:1, dentro del periodo).
- **`map_conn_err` + `From<FrameError>` reclassificación** (`2de84fe`):
  - `ConnectionClosed | LocallyClosed | ApplicationClosed | Reset` → `TransportError::Closed`.
  - `TimedOut` → variante nueva `TransportError::TimedOut`.
  - IO `UnexpectedEof | BrokenPipe | ConnectionReset | ConnectionAborted` → `Closed`.
  - 12 sitios `*_inbound_loop` / `streams_accept_loop` (daemon + companion) absorben `TimedOut` con misma rama de `Closed` → fuera el warn floor (`early eof`, `connection lost`, `Conn cycle 3s`).

### "Cursor pesado" — cerrado (sesión 2026-06-24)

Diagnóstico real con telemetría: **loss=0% RTT 7ms** sostenido. Loss descartado. Causa fue el clamp 5mm.

- **Drop clamp 5mm** (`6ea62c7`). Removía 1-2mm de movimiento por sample en drags rápidos → cursor "pesado". Con Kotlin historical-samples + 0% loss, ya no hacía falta defenderse contra missing samples.
- **Re-introducción multi-emit subframe** (`88c0bcb`). Drop clamp solo destapó: libinput `Touch jump detected and discarded` 5x por sesión + rate-limit silencioso → tirones visibles. Fix definitivo: clamp 6mm pero, en lugar de soltar el overshoot, `subframe_path` interpola línea recta con steps `<= max_delta`, emitiendo cada sub-frame con `POSITION + PRESSURE + ToolType + Major/Minor + Orientation + BTN_TOUCH + SYN_REPORT` consecutivo. libinput recibe secuencia válida cubriendo distancia exacta. 4 unit tests (`cargo test -p ansync-input --features uinput subframe`).
- **Discovery filter loopback / IPv6 link-local** (`0995fa4`). `HostDiscovery.onServiceResolved` rechaza `isLoopback || isAnyLocal || isMulticast || (Inet6 && isLinkLocal)`. `HostDialer.tryDirectFallback` parsea IPv6 con bracket, valida via `InetAddress.getByName`, filtra. Ya no perdíamos 4s probando `fe80::…%wlan0`.
- **ConnGuard race** (`5546e4f`). Dos `streams_accept_loop` simultáneos (viejo + nuevo durante redial) competían por `CONNECTED` atomic. Fix: guard carga `Arc<QuicConnection>`, en drop chequea `Arc::ptr_eq` contra `state_slot().session.conn`. Solo flippea si sigue siendo current → notif "Looking..." ya no aparece con session viva.
- **Outbound input dead-stream** (`5546e4f`). `nativeSendInputMessage` ahora clarea `*guard = None` si `send` falla → next call reabre Input stream sobre mismo conn. Antes: 1 transient send fail ⇒ todos los siguientes muertos contra stream zombi.

### "Cursor chill se endura" (slow-finger sticky) — cerrado

Síntoma residual reportado en mid-session: cursor fluido pero se "endurece" en moves lentísimos. Hipótesis principal: libinput `accel-profile=adaptive` aplica multiplier <1.0 a velocidades bajas (precision mode by design). Fix compositor-side: `gsettings set org.gnome.desktop.peripherals.touchpad accel-profile 'flat'` (GNOME), `accel_profile flat` en sway/wayfire/hyprland config, KDE Settings → Mouse → Pointer acceleration = None. No requiere cambios en ansync.
