# ansync — Claude instructions

Reescritura de scrcpy en Rust con scope ampliado: mirror de pantalla, control bidireccional, transferencia y montaje FUSE de archivos, cámara/micrófono virtuales, audio bidireccional, clipboard sync, descubrimiento mDNS, pairing seguro con Ed25519 + Noise XX sobre QUIC.

**Lee [`PLAN.md`](./PLAN.md) para el roadmap canónico, decisiones cerradas y estado de cada step.** Este archivo es la guía rápida para sesiones nuevas.

## Layout

```
crates/      librerías por dominio, todas con traits + impls detrás de feature flags
bins/        ansyncd (daemon + GUI eframe) + ansyncctl (CLI)
android/     companion app Kotlin (Gradle KTS) — aún no creada
nix/         módulos NixOS / home-manager y derivaciones de build
```

## Reglas duras

- **Traits primero**. Cada backend (`AudioBackend`, `VirtualCameraSink`, `VirtualInputDevice`, `Transport`, `Discovery`, `RemoteFsBackend`, `ClipboardBackend`, `PermissionsStore`) es un trait. Impls concretas detrás de feature flags. Esto permite sumar ALSA/JACK/PipeWire-camera/BT-HID/relay-NAT más adelante sin tocar al resto.
- **Permisos por dispositivo**. Cualquier acción que toque hardware, red u IO chequea `DevicePermissions` antes de proceder. Sin flag = `Error::PermissionDenied(Permission)`. Persistencia: `$XDG_CONFIG_HOME/ansync/devices/{id}.toml`.
- **Sin ffmpeg**. Codecs vía `ferricast-encoder` / `ferricast-decoder` (NVENC, VAAPI, openh264). HEVC se extiende en ferricast — ver Step 5 del roadmap.
- **Sin OpenSSL**. `rustls` con `default-features = false`, root store vacío, custom verifier que pinea al pubkey Ed25519 del peer.
- **Sin `#[allow(unused_*)]`**. Si algo no se usa, eliminarlo. Si la visibilidad rompe la signature pública, ajustar `pub(crate)` del módulo, no re-exportar para silenciar.
- **`tracing` → `tracing-journald`** en el daemon. Sin `println!` salvo en el CLI.
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

**Step 6 completo**: `HostDecoder` ahora instance-owned (slot `Arc<Mutex<Option<CapturedFrame>>>`), `DecodedFrame` con `stride` + `Bgra8`/`Rgba8`. `ansync_video::feed::AnnexBFile` itera Access Units desde `.h264`/`.h265` para alimentar al decoder sin companion Android. `ansyncd::mirror_window` levanta `eframe` con `Renderer::Wgpu`, convierte NV12/I420/BGRA/RGBA → `ColorImage` y sube vía `Context::load_texture`. Flag `--play-file PATH` + módulo `mirror_window` viven detrás del feature **`dev-playback`** (off por default) — prod no linkea `eframe`/`egui`/`ansync-video` ni acepta el flag. `flake.nix` exporta `LIBCLANG_PATH` para bindgen.

**Step 7 en progreso**:
- **7a cerrado** — `ansync_input::uinput` con `Keyboard`, `Mouse`, `Touchscreen` (MT-B), `Stylus` (pen + tilt), `Gamepad` (XInput-like). Cada uno abre `/dev/uinput` en `create()`, advertiza evbits/keybits/absbits, y traduce `crate::InputEvent` → `input_linux::sys::input_event`. Feature `uinput` activa la pieza. Bus virtual + pid.codes vendor + product id por kind para que `udevadm` distinga el tipo. Ship-ready: `bins/ansyncd/contrib/60-ansync-uinput.rules` + `nix/uinput.nix` (kernel module + udev rule) — Step 14 los wirea al módulo NixOS final.
- **7b-1 cerrado** — `ansync_input::InputSession` orquesta los 5 uinput devices por peer. Lazy construction (primer event del kind dispara `create()`), permission gate per-event contra `Permission::InputFromDevice` (revoke mid-session corta el siguiente event sin tirar el stream QUIC). `InputDeviceFactory` trait + `UinputFactory` impl detrás del feature `uinput`. `wire_to_event` mapea `proto::InputMessage → input::InputEvent`.
- **7b-2 cerrado** — `Ed25519AnyPeerVerifier` (trait `TrustedPeers`) + `QuicTransport::bind_any` aceptan cualquier peer cuyo Ed25519 pubkey pase el predicate. Daemon-core: `PeerStoreTrust` chequea contra el store por handshake; `Daemon::run` bind antes de mDNS para anunciar el puerto real, levanta accept loop, per-conn handler resuelve `DeviceId` desde pubkey + carga el `StoredPeer`, monta `InputSession` por peer detrás de `Arc<Mutex>`, spawn `input_stream_loop` por cada `StreamKind::Input` (postcard `InputMessage` por frame). Otros stream kinds aceptados + logged "no wired yet" sin tirar conexión. `Capabilities::INPUT_FROM_DEV` default en `DaemonConfig`. `PeerStore` ahora `Clone + Debug`.
- **7c cerrado** — Companion scaffold en `android/`: Gradle KTS + version catalog (AGP 8.5.2 / Kotlin 2.0.20 / compileSdk 35 / minSdk 26 / Java 17). Manifest declara permisos LAN + foreground (mediaProjection) + accessibility. Stubs: `MainActivity` Compose, `AnsyncCompanionService` foreground con notif channel, `AnsyncAccessibilityService` con static handle pattern. Build no validable desde nix shell (falta Android SDK + Gradle wrapper) — usuario corre `gradle wrapper && ./gradlew assembleDebug` localmente.
- **7d-1 cerrado** — Cdylib `ansync_companion_native` en `android/` (fuera del workspace host por target diferente). JNI surface: `nativeInit / nativeOpenConnection / nativeSendVideoChunk / nativePollInputMessage / nativeClose`. Tokio runtime estático + android_logger; sesión en `Mutex<Option<CompanionSession>>`. Gradle integra vía Mozilla `rust-android-gradle 0.9.6`; `cargoBuild` task encadenada con `mergeJniLibFolders`. Versiones repineadas a la imagen `rust-android:1.90-sdk-36`. `AnsyncCompanionService.onCreate` llama `NativeBridge.nativeInit()`, `onDestroy` llama `nativeClose()`. Build: `docker run --rm -v "$(pwd):/src" -w /src rust-android:1.90-sdk-36 -p android assembleDebug`.
- **7d-2 cerrado** — Companion native ahora dial real. `nativeInit(filesDir)` carga/genera Ed25519 identity en `{filesDir}/identity.key`. `nativeOurPubkeyHex()` para el pairing UI. `nativeOpenConnection(host, port, daemonPubkeyHex)` → `QuicTransport::connect` con pinning contra pubkey del daemon, apre `Video` + `Input` streams. Recv-loop async empuja a `mpsc::UnboundedSender`; `nativePollInputMessage` consume. `nativeSendVideoChunk(chunk, ptsUs)` `write_frame` sobre el Video stream. Compila standalone (`[workspace]` vacío en `android/Cargo.toml`).
- **7d-3 cerrado** — `CaptureSession` Kotlin orquesta `MediaProjection` + `VirtualDisplay` + `MediaCodec` AVC encoder Baseline (1080p60 / 8 Mbps / I-frame 5 s default). Drain thread dedicado lee buffers y los pushea por JNI. `AnsyncCompanionService` consume `ACTION_START_CAPTURE` con el Intent de MediaProjection y arranca/destruye el session. `MainActivity` Compose muestra fingerprint del pubkey + botón "Start screen capture". Falta pairing flow (host discovery + accept fingerprint), eso es 7d-4 (no estaba originalmente en el plan, lo voy a sumar).
- **7e cerrado** — `AnsyncAccessibilityService` con `HandlerThread` dedicado polling `nativePollInputMessage()`. Wire format Rust→Kotlin: flat tag+payload binary (`encode_for_kotlin` en lib.rs ↔ `WireInputMessage.decode` en Kotlin) — schema en dos lugares, comentario forzando cambios paralelos. TouchSlot → `dispatchGesture(GestureDescription.StrokeDescription)` 16 ms. KeyPress / system actions wired si se necesitan a futuro. Mouse / Gamepad descartados silenciosamente (no aplica al device).
- **Step 7 cerrado**. Pendiente: testing real con companion en device (no validable desde dev shell).
- **Step 8 cerrado** — `ansync_files::transfer` con `send_file`/`receive_file` state machines (Offer + sha256 → Accept/Reject → Chunks 256 KiB → Complete + verify). `InboundPolicy` trait + `AutoAcceptPolicy` defaultea a `{root}/{peer_id}/{safe_name}`. Permission gates `FilesSend`/`FilesReceive` re-chequeados por chunk. Daemon accept `StreamKind::Files` → `files_stream_loop`. Companion cdylib expone `files_accept_loop` con `PermissivePermissions` in-memory store. `ansyncctl push <id> <path> [--addr] [--seconds]` direct dial bypass D-Bus.
- **Step 9 host side cerrado (9a-9c)** — `FsOpMessage` extendido (Create/Unlink/Rename/Truncate/Chmod + Ok). `ansync_files::fs::{client,cache,fuse_mount}`: sequential RPC client + TTL metadata cache (stat 5s, readdir 5s, negative 1s, sin cache de contenido) + `FuseMount<S>: fuser::Filesystem`. Daemon auto-mount al connect si `Permission::FilesMount` ON, monta en `$XDG_RUNTIME_DIR/ansync/mounts/{name}/`. `nix/fuse.nix` partial. 4 tests del cache pasan.
- **Step 9 cerrado end-to-end** — Companion native `streams_accept_loop` demuxa Files/Fs; `fs_serve_loop` sequencial postcard ↔ tag-binary bridge. JNI `nativePollFsRequest()` blocking + `nativeFsReply()`. Kotlin `FsOpCodec` espejo + `AnsyncFsServer` worker thread → SAF `DocumentsContract` ops. `stat`/`readdir`/`open`/`read`/`close` shipping; mutaciones devuelven ENOSYS (follow-up). `MainActivity` picker `ACTION_OPEN_DOCUMENT_TREE` + `takePersistableUriPermission` + SharedPreferences persist. `AnsyncCompanionService` arranca server si hay tree URI.
- **Step 9.5 arrancado (glue integration)**:
  - **9.5a cerrado** — Renderer `MirrorApp`/`FrameSlot`/`run` movido a `ansync_video::sink_egui`. Daemon-core `video_stream_loop` decode H.264 → push a slot per-peer en `MirrorRegistry`. `ansyncd` lib-side ahora solo tiene el feeder Annex-B dev-only.
  - **9.5b cerrado** — `DaemonAction::{ShowScreen,HideScreen}` enum + `UnboundedSender` en `DaemonState`. D-Bus `Device.ShowScreen` envía action; `action_loop` spawnea thread con `sink_egui::run(title, slot)`. Window thread separado del tokio runtime. Ya se puede probar: pairing manual + `dbus-send` para ShowScreen + companion empujando Video.
  - **9.5c cerrado** — Companion `streams_accept_loop` maneja Input inbound → mpsc → AccessibilityService. Convención: opener escribe, accepter lee. `nativeOpenConnection` ya no pre-abre Input.
  - **9.5d cerrado** — `ShowScreen` action handler abre Input outbound; `MirrorApp` mapea pointer egui → `InputMessage::TouchSlot` con coords absolutas; `input_writer_loop` postcard + write_frame.
  - **9.5f cerrado** — Cable pairing companion side: `pair_host_via_adb` dispara `adb shell am broadcast` post-reverse → `PairingReceiver` extrae port → `nativePairOverCable(port, name)` → `bootstrap_companion` sobre TCP 127.0.0.1:port → persist `host_pubkey_hex` + `host_name` en SharedPreferences. Sin AlertDialog: cable es security guarantee. `MainActivity` muestra paired host.
  - **9.5e cerrado** — `TouchpadActivity` Compose full-screen MotionEvent capture → `WireInputMessage.encode()` tag-binary → `nativeSendInputMessage(blob)`. Native `decode_input_from_kotlin` → postcard → outbound Input stream lazy-open. Touch-down → MouseButton{1,true}, move → MouseMove{dx,dy}, up → MouseButton{1,false}.
  - **9.5e+ cerrado (device→host completo)** — TouchpadActivity gana long-press (>450 ms estacionario) → `MouseButton{2}`, 2-finger drag → `MouseWheel` (Δy /8 wheel ticks, Y-up positivo), 2-finger tap (<200 ms, sin movimiento) → `MouseButton{3}`. TOOL_TYPE_STYLUS detecta el pen y emite `Stylus` (x/y a 0..32767 escalado al canvas, pressure 0..8191, tiltX/tiltY de `AXIS_TILT`+orient, BUTTON_STYLUS_PRIMARY/SECONDARY → btn bits). Hardware kbd via `dispatchKeyEvent` override → `KeyPress`; soft IME via `BasicTextField` invisible + onValueChange sintetiza press/release por char (auto-shift mayúsculas + ASCII shifted punctuation). Nueva `GamepadActivity` + `GamepadTile`: overrides `dispatchKeyEvent` + `dispatchGenericMotionEvent` (SOURCE_JOYSTICK axes X/Y/Z/RZ + LTRIGGER/RTRIGGER fallback BRAKE/GAS) → emite `Gamepad` con bitmask 11-button mirror de `GP_BTN_LIST` (A/B/Y/X/L1/R1/Select/Start/Mode/ThumbL/ThumbR). DPAD + L2/R2-como-button drop silencioso (proto sin slots de hat axes, L2/R2 cubierto via triggers). Nuevo `KeycodeMap.kt` traduce Android KEYCODE_* → evdev KEY_*. WireInputMessage Kotlin encode arms Stylus/Gamepad ya no tiran. Rust `decode_input_from_kotlin` cubre tags 5+6 con helpers `take_u16`/`take_i16`.
- **Step 9.5 cerrado end-to-end**. Ya se puede:
  1. Daemon corriendo (`ansyncd` con FUSE + uinput perms + identity inicial).
  2. `ansyncctl pair --serial XXX` con Android conectado vía USB → auto-wake del companion vía broadcast → bootstrap → ambos lados persistidos.
  3. Restart daemon (D-Bus surface ve nuevo peer).
  4. Companion app → "Start screen capture" → MediaProjection grant → push H.264 → daemon decode + slot.
  5. `dbus-send` o `gdbus call /org/gameros/Ansync1/Device/{id} org.gameros.Ansync1.Device.ShowScreen` → ventana eframe + outbound Input.
  6. Click en ventana host → TouchSlot al Android → AccessibilityService dispatchGesture.
  7. Companion "Open touchpad" → MotionEvent → daemon uinput Mouse.
  8. FUSE auto-mount si `files_mount` perm on; ls del mount → SAF.
  9. `ansyncctl push id path` → transferencia + sha256 verify.
- **Post-9.5 gap closers (UX scrcpy-level)**:
  - **D-Bus dynamic registration** — `Manager.RefreshPeers()` D-Bus method; `ansyncctl pair` lo llama post-store.put. No más restart del daemon después de pair.
  - **Auto-install APK durante pair** — `pair_host_via_adb` ahora chequea `pm list packages` y corre `adb install -r -g` si el companion no está. CLI flag `--apk` o env `ANSYNC_COMPANION_APK` o default `/usr/share/ansync/companion.apk`. UX idéntica a scrcpy modulo path al APK.
  - **Companion mDNS + Connect button** — `HostDiscovery.kt` wrappea `NsdManager` con `WifiManager.MulticastLock` (mandatorio en Android). `MainActivity` matchea paired pubkey con hosts descubiertos y muestra botón "Connect to X (IP)" que dispara `nativeOpenConnection`.
- **Step 10 cerrado** — Camera v4l2loopback end-to-end:
  - Proto: `CameraConfig {camera_id, w, h, fps, bitrate_kbps, codec, aspect, stabilization}` + `CameraAspect{Crop,Letterbox,Stretch}`. `ControlMessage::StartCamera(CameraConfig)` reemplaza el stub. `StreamKind::Camera` tag 0x07.
  - `ansync_camera::V4l2LoopbackSink` impl `VirtualCameraSink` (feature `v4l2loopback`): auto-discover scan `/dev/video*` por `V4L2_CAP_VIDEO_OUTPUT` + `with_path` override; set_format NV12 / YUYV / MJPG; `write_frame` raw → `libc::write` al fd (v4l2loopback acepta `write(2)` directo). Card label fijo "Ansync" via modprobe option.
  - D-Bus: `Device.StartCamera(camera_id, w, h, fps, bitrate_kbps, codec, aspect, stabilization)` + `StopCamera()`. Codec str `h264|h265`, aspect str `crop|letterbox|stretch`. Disparan `DaemonAction::{StartCamera,StopCamera}`.
  - `daemon-core`: `CameraRegistry` per-peer (sink + JoinHandle + frame_tx mpsc). `handle_start_camera` chequea `Permission::CameraVideo`, abre `StreamKind::Control` outbound, manda postcard `Envelope{Message::Control(StartCamera(cfg))}`, spawn-ea `camera_decode_loop` (HostDecoder NV12 → sink). Accept `StreamKind::Camera` inbound demuxa al `frame_tx` del entry. Disconnect tear-down (abort handle + sink.unregister).
  - Companion native: JNI `nativePollCameraControl` + `nativeSendCameraChunk` (lazy `StreamKind::Camera` outbound) + `nativeStopCameraStream`. `streams_accept_loop` demuxa `StreamKind::Control` → `control_recv_loop` decoda Envelope/Message Control → tag-binary blob para Kotlin.
  - Companion Kotlin: `CameraSession` Camera2 + MediaCodec AVC/HEVC con Surface input (zero-copy sensor → encoder). `pickOutputSize` matchea cuello bajo, `CONTROL_AE_TARGET_FPS_RANGE` fija fps, `CONTROL_VIDEO_STABILIZATION_MODE_ON` opcional. `AnsyncCompanionService` arranca HandlerThread `ansync-cam-ctrl` que polea native + dispatch Start/Stop. `WireCameraControl.kt` espejo. Manifest: `CAMERA` + `FOREGROUND_SERVICE_CAMERA`; service foregroundServiceType `mediaProjection|camera`.
  - `Capabilities::CAMERA_VIDEO` default-on en `DaemonConfig`.
  - `nix/v4l2loopback.nix` partial: `extraModulePackages = [ kernelPackages.v4l2loopback ]` + modprobe options (`devices=1 video_nr=10 card_label="Ansync" exclusive_caps=1`) + udev rule grupo `video`. Step 14 importa.
- **Step 11 cerrado** — Audio bidireccional cpal ↔ AudioRecord/AudioTrack:
  - `ansync_audio::CpalBackend` (feature `cpal-backend`) — cpal habla PipeWire via la ALSA shim, evita pipewire-rs FFI. `CpalSource` capture + `CpalSink` playback. 48 kHz / stereo / S16LE en ambos lados.
  - `ControlMessage::StartAudioRoute{direction}` + `StopAudioRoute`. `AudioStreamInit` header en primer frame de `StreamKind::Audio`.
  - Daemon-core: `AudioRegistry` per-peer, perm gates `AudioIn`/`AudioOut`. `handle_start_audio` abre Control + provisions sink/source/streams. `audio_render_loop` drena inbound → `CpalSink`; `audio_pump_loop` drena `CpalSource` → outbound.
  - D-Bus `Device.StartAudioRoute(direction)` + `StopAudioRoute` + `StartMicrophone`/`StopMicrophone` sugar.
  - Companion native: `nativePollAudioControl` / `nativeSendAudioChunk` / `nativePollAudioChunk` / `nativeStopAudioStream`. `streams_accept_loop` demuxa `StreamKind::Audio` inbound.
  - Companion Kotlin: `AudioRouter` w/ `AudioRecord` (mic→host) + `AudioTrack` (host→device). `WireAudioControl.kt` mirror del encoder Rust. Manifest gana `RECORD_AUDIO` + `MODIFY_AUDIO_SETTINGS` + `FOREGROUND_SERVICE_MICROPHONE`; service foregroundServiceType += `microphone`. MediaSession widget queda pending nice-to-have.
- **Step 12 cerrado** — Clipboard sync Wayland ↔ Android:
  - `ansync_clipboard::WaylandClipboard` (feature `wayland`) wrappea `wl-clipboard-rs` con spawn_blocking.
  - `StreamKind::Clipboard` tag 0x08. `ClipboardMessage::Text|Blob` (ya existía en proto). Inbound perm gate `ClipboardIn`. Outbound via `DaemonAction::SyncClipboard` + perm gate `ClipboardOut`.
  - D-Bus `Device.SyncClipboard()`. Companion JNI `nativeSendClipboardText` + `nativePollClipboardText`. Blob payloads ignored por ahora (text only).
  - Kotlin `ClipboardBridge` polea native + `ClipboardManager.setPrimaryClip`. `pushToHost()` lee primaryClip + manda JNI. `AnsyncCompanionService` lifecycle.
  - `Capabilities::CLIPBOARD` default-on.
- **Step 13 cerrado (scaffold)** — BT-HID via bluer:
  - `ansync_input::bt_hid::BtHidFactory` impl `InputDeviceFactory` (feature `bt-hid`). `BtHidDevice` abre `bluer::Session` + adapter + powered=true, loguea HID reports. Profile registration (SDP + L2CAP control/interrupt) deferred.
- **Step 14 cerrado** — Nix module + crane:
  - `nix/package.nix` crane build, instala udev rule + systemd unit a `$out`.
  - `nix/module.nix` consolida imports de uinput/fuse/v4l2loopback partials. Opciones `services.ansync.{enable,user,package,extraGroups}`. Suma user a `input`/`video`/`fuse`. Systemd user unit con sandboxing.
  - `nix/hm-module.nix` fallback home-manager para no-NixOS.
  - `flake.nix` expone `nixosModules.default`, `homeManagerModules.default`, `packages.default`, apps `ansyncd`/`ansyncctl`.
  - `flake.nix` dev-shell gana `alsa-lib`.
- **Step 15 cerrado** — README expandido: status table, arquitectura ASCII, install NixOS + manual, pair workflow, surface D-Bus completa, ejemplos gdbus, troubleshooting, logs.
- **Step 16 cerrado** — Pure-Rust ADB:
  - `Command::new("adb")` de `pairing/cable.rs` migradas a `adb_client::ADBServer` + `ADBServerDevice::{reverse, reverse_remove_all, shell_command, install}`. Sync API → spawn_blocking. Sigue requiriendo adbd en host.
- **Step 17 cerrado** — APK auto-fetch:
  - `release::fetch_latest_companion()` GET `api.github.com/repos/SergioRibera/ansync/releases/latest` via reqwest `rustls-tls`. Picks asset `companion*.apk`.
  - Cache en `$XDG_CACHE_HOME/ansync/companion-{tag}.apk`, size + SHA-256 verify (digest del release API; warning skip si ausente).
  - `query_installed_version` parsea `dumpsys package` por `versionName=`.
  - `ansyncctl pair` ahora: si no hay --apk/env/path Y companion missing → auto-fetch + install. Override sigue funcionando.
- **Roadmap completo.** Ver `PLAN.md` para tabla final.

**Triaje UX post-v1 (sesión 2026-06-15)**:
- **U1 cerrado** — `StreamKind::Hello` (tag 0x0a) one-shot bidi post-handshake. Host envía `gethostname(2)` + caps; companion envía `Build.MANUFACTURER + " " + Build.MODEL` via `nativeSetDeviceName`. Daemon `hello_inbound_loop` refresca `StoredPeer.name`; companion stash en `last_host_name` + `nativePollHostName` + MainActivity LaunchedEffect persiste `PREF_HOST_NAME`. `Device.Name` D-Bus surface hostname real.
- **U2 cerrado** — `ConnState{Disconnected,Pairing,Authenticated,Active}` en `ansync_dbus::state` + registry per-device en `DaemonState`. `Device::emit_state_changed` helper dispara auto-generated `PropertiesChanged(State)` + custom `Manager.DeviceConnectivityChanged(id,state)`. `handle_connection` transiciona Authenticated → Active (post-Hello) → Disconnected.
- **U3 cerrado** — pair fail diagnosed end-to-end. Cuatro bugs:
  1. `adb_client::reverse` no instala el listener en adbd → shell-out a binario `adb` para reverse / reverse --remove-all en `pair_host_via_adb`.
  2. `bootstrap_host` no flusheaba TCP antes de drop → companion "early eof". Fix: `stream.flush().await + stream.shutdown().await` post-Ack.
  3. `adb_client::shell_command` shell_v2 entrega framing bytes mezclados → strict line match fallaba. Fix: substring match en `companion_installed` + `query_installed_version`.
  4. `PairingReceiver` Kotlin usaba `name` extra (= host_name) como propio → companion enviaba "ansync-host" como su nombre. Fix: usar `${Build.MANUFACTURER} ${Build.MODEL}` siempre (U1 Hello frame auto-corrige en próximo connect, pero pair store quedaba con nombre feo).
- Verificado: pair end-to-end OK; PeerStore persiste; `Manager.RefreshPeers` registra path D-Bus sin restart.
- **U4 cerrado** — Companion headless:
  - **U4a**: drop MainActivity + Compose UI + launcher icon. Translucent shims: `PermissionsBootstrapActivity` (walks POST_NOTIFICATIONS / RECORD_AUDIO / SAF / Accessibility / NotificationListener), `GrantScreenCaptureActivity` (MediaProjection picker), `GrantStorageActivity` (re-pick SAF tree). Service `onCreate` kickea bootstrap si `PREF_GRANTS_BOOTSTRAPPED` off. `PairingReceiver` arranca service post-pair atomic. `foregroundServiceType=dataSync|mediaProjection|camera|microphone` con `promoteForegroundType()` switching on stream start (Android 14+ rechaza media-tipos sin token activo). `Prefs.kt` central.
  - **U4b**: 4 QSTiles (`MirrorTile`, `TouchpadTile`, `MicShareTile`, `AudioSinkTile`) under `org.gameros.ansync.tile.*`. State persistido en SharedPreferences. Tiles wiring → `ACTION_{START,STOP}_{MIC_SHARE,AUDIO_SINK,CAPTURE}`. `startActivityAndCollapse` API 34+ con PendingIntent.
  - **U4c**: `BootReceiver` (BOOT_COMPLETED + LOCKED_BOOT_COMPLETED + MY_PACKAGE_REPLACED). `HostDialer` con `ConnectivityManager.NetworkCallback` (Wi-Fi / Ethernet) + `HostDiscovery` mDNS reuse + exponential backoff (1s→60s). Auto-redial post-network-up.
  - **U4d**: Notif persistente state-driven con action buttons per-stream (Stop mirror / mic / PC audio / camera). `refreshNotification()` desde cada lifecycle helper. Cleanup automático del "tap to grant" notif on capture start.
  - **U4e deferred**: WiFi pair PIN scope grande (PIN gen + activity + native TCP listener + host CLI + Pin protocol). Cable USB pair sigue funcional.
- **U5 cerrado** — `ControlMessage::{RequestScreenCapture, StopScreenCapture}` end-to-end. Daemon `action_loop::ShowScreen/HideScreen` mandan al companion via Control stream one-shot (`send_control` helper). Companion native `capture_ctrl_rx` + JNI `nativePollCaptureControl`. Companion service `ansync-cap-ctrl` worker thread → tag 0 = `requestCaptureFromUser()` (high-priority notif "tap to grant"), tag 1 = `stopCapture()`. Auto-connect end-to-end via U4c HostDialer.
- **Retoques finales (post-roadmap)**:
  - **Bloqueantes cerrados**: R1 (APK upgrade flow: `--auto-upgrade` / `--skip-upgrade-check` + prompt), R2 (audio loops perm gates per-chunk), R4 (notifications wired: `StreamKind::Notifications` 0x09 + D-Bus signals `Device.NotificationPosted/Removed` + companion `NotificationForwarder` + JNI `nativeSendNotificationPosted/Removed`), R9 (`nix build .#default` verde — ferricast pasó a git dep `db0f7531`, drop path dep), R12 (cleanup auto-cerrado por R2).
  - **Cerrados además**: R5 (SAF write/create/unlink/rename/truncate via DocumentsContract; chmod queda ENOSYS; cross-dir rename → EXDEV), R6 (`InputBackend{Uinput,BtHid}` enum + factory selection; HID Boot reports KB 8-byte, Mouse 4-byte, Gamepad 8-byte; L2CAP PSM 0x11/0x13 SeqPacket listeners; SDP Profile1 registration documentado como follow-up), R11 (clipboard blob bidi: image MIMEs via MediaStore.Images.insert + `nativeSendClipboardBlob` / `nativePollClipboardBlob`).
  - **Pendientes** (deliberately skipped): R3 (botón push clipboard host), R7 (MediaSession widget audio), R10 (sensors stream). R8 (v4l2loopback per-peer card_label) sigue WONTFIX upstream.

Ver `PLAN.md` § Roadmap para la lista completa.

## Convenciones de continuidad

Al retomar en una sesión nueva:

1. Leer `PLAN.md` y este archivo.
2. Identificar el primer step sin `[x]`.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` en `PLAN.md`, actualizar el sección "Estado actual" de este archivo, y commitear con un single-line.
