# ansync

Integración fluida Android ↔ Linux. Pantalla, archivos, cámara, micrófono, audio, inputs — todo sobre la misma red, sin cable.

## Estado

Pre-alpha. En desarrollo activo. Roadmap completo en [`PLAN.md`](./PLAN.md).

## Features previstas

- Mirror de pantalla Android en Linux con baja latencia (decode HW: NVENC, VAAPI)
- Control bidireccional: usá el mouse/teclado de la PC en Android, o usá Android como teclado/stylus/gamepad/touchpad para la PC
- Transferencia de archivos bidireccional + montaje FUSE del filesystem Android
- Cámara y micrófono virtuales en Linux (v4l2loopback + PipeWire)
- Audio bidireccional con widgets de control en la barra de notificaciones Android
- Clipboard sync configurable por dispositivo
- Descubrimiento automático en LAN vía mDNS
- Cifrado E2E con QUIC + rustls + pinning Ed25519

## Arquitectura

Un único daemon (`ansyncd`) expone un protocolo D-Bus (`org.gameros.Ansync1`) que cualquier aplicación de escritorio puede consumir. El mismo daemon dibuja la ventana de mirror con `eframe` + `egui` + `wgpu`. CLI de control: `ansyncctl`.

Todos los backends (audio, cámara virtual, input, transporte, descubrimiento, filesystem, clipboard) están detrás de traits para poder sumar implementaciones (ALSA, JACK, PipeWire-camera, BT-HID, relay NAT, etc.) sin tocar el resto del sistema.

Codecs vía [ferricast](../../ferricast) — NVENC, VAAPI, openh264 SW fallback. Cero ffmpeg, cero OpenSSL.

## Permisos por dispositivo

Cada device pareado tiene un set de flags configurables (mirror, cámara, mic, mount, clipboard, inputs, etc.) persistido en `$XDG_CONFIG_HOME/ansync/devices/{id}.toml`. Cada acción del daemon verifica el flag antes de ejecutarse.

## Build

Con Nix (recomendado):

```sh
nix develop
cargo build --release
```

## Requisitos del sistema

- Linux con PipeWire, v4l2loopback (módulo cargado), FUSE3, BlueZ, D-Bus
- Android 10+ con la companion app (Kotlin, en desarrollo)
- GPU con NVENC o VAAPI para decode acelerado (opcional, hay fallback CPU)

## Licencia

MIT OR Apache-2.0
