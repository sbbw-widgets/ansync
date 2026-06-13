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

**Step 6 completo**: `HostDecoder` ahora instance-owned (slot `Arc<Mutex<Option<CapturedFrame>>>`), `DecodedFrame` con `stride` + `Bgra8`/`Rgba8`. `ansync_video::feed::AnnexBFile` itera Access Units desde `.h264`/`.h265` para alimentar al decoder sin companion Android. `ansyncd::mirror_window` levanta `eframe` con `Renderer::Wgpu`, convierte NV12/I420/BGRA/RGBA → `ColorImage` y sube vía `Context::load_texture`. Nuevo flag `--play-file PATH` en `ansyncd` corre solo decode + window (D-Bus skip). `flake.nix` exporta `LIBCLANG_PATH` para bindgen.

**Próximo**: Step 7 — `ansync_input` uinput (Android como kbd/touch/stylus/gamepad del PC) + reverse vía AccessibilityService en el companion Kotlin. Acá arranca formalmente la app Android en `android/` — el companion se vuelve necesario para producir streams reales de input + capture H.264 (Step 6 ya quedó probado vía `--play-file`).

Ver `PLAN.md` § Roadmap para la lista completa.

## Convenciones de continuidad

Al retomar en una sesión nueva:

1. Leer `PLAN.md` y este archivo.
2. Identificar el primer step sin `[x]`.
3. Confirmar con el usuario antes de empezar pasos de implementación.
4. Al terminar un step, marcarlo `[x]` en `PLAN.md`, actualizar el sección "Estado actual" de este archivo, y commitear con un single-line.
