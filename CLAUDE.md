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
- **Próximo (7b-2)** — Bind QUIC server en `daemon-core`, accept loop con peer auth contra `PeerStore`, demux por `StreamKind`; el stream `Input` se loopea contra `InputSession::dispatch`.
- Después (7c–e) arranca companion Android en `android/`.

Ver `PLAN.md` § Roadmap para la lista completa.

## Convenciones de continuidad

Al retomar en una sesión nueva:

1. Leer `PLAN.md` y este archivo.
2. Identificar el primer step sin `[x]`.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` en `PLAN.md`, actualizar el sección "Estado actual" de este archivo, y commitear con un single-line.
