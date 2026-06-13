# ansync

Integración fluida Android ↔ Linux. Pantalla, archivos, cámara, micrófono, audio, inputs, clipboard — todo sobre LAN, sin cable.

## Estado

Pre-alpha. Funcional end-to-end para los flujos principales. Roadmap completo en [`PLAN.md`](./PLAN.md).

| Step | Tema | Estado |
|------|------|--------|
| 1–6  | Skeleton + transport + crypto + discovery + video decode | ✅ |
| 7    | Input virtual host (uinput) + companion Accessibility | ✅ |
| 8    | File transfer push/pull | ✅ |
| 9    | FUSE mount + SAF integration | ✅ |
| 9.5  | Integration glue (eframe window + cable pairing + D-Bus) | ✅ |
| 10   | Camera v4l2loopback + Camera2/MediaCodec | ✅ |
| 11   | Audio bidireccional (cpal/PipeWire ↔ AudioRecord/AudioTrack) | ✅ |
| 12   | Clipboard sync con perm gates | ✅ |
| 13   | BT-HID secundario via bluer | scaffold |
| 14   | Nix module + crane derivation | ✅ |
| 15   | Docs + README | ✅ |
| 16   | Pure-Rust ADB (`adb_client`) | ✅ |
| 17   | APK auto-fetch desde GitHub releases | ✅ |

## Features

- **Mirror** de pantalla Android → Linux con decode HW (NVENC → VAAPI → openh264 SW fallback).
- **Control bidireccional**:
  - PC → Android: pointer/keyboard via Accessibility (`dispatchGesture`).
  - Android → PC: keyboard / mouse / touchscreen MT-B / stylus / gamepad XInput-like via uinput.
- **Transferencia de archivos** con sha256 verify + chunks de 256 KiB.
- **FUSE mount** del FS Android (SAF backend en companion). Auto-mount al connect si `files_mount` ON.
- **Cámara virtual** v4l2loopback con frames de Camera2 + MediaCodec H.264/H.265.
- **Audio bidireccional**: cpal (Linux PipeWire/ALSA) ↔ AudioRecord/AudioTrack. 48 kHz stereo S16LE.
- **Clipboard sync** Wayland ↔ Android ClipboardManager, con gates por device.
- **Descubrimiento** automático mDNS (`_ansync._udp.local.`).
- **Pairing** cable ADB one-shot → llave Ed25519 long-term persistida. Sin PIN: el cable es la garantía de seguridad.
- **Crypto E2E**: QUIC (`quinn`) + `rustls` con pinning Ed25519. Cero OpenSSL, cero ffmpeg.

## Arquitectura

```
┌──────────────────────────────────────────────────────────┐
│  ansyncd  (daemon + GUI eframe)                          │
│  ├── QUIC server (rustls + Ed25519 pinning)              │
│  ├── mDNS announcer                                       │
│  ├── D-Bus surface (org.gameros.Ansync1)                  │
│  ├── MirrorRegistry / CameraRegistry / AudioRegistry      │
│  └── Per-peer: input session (uinput) / FUSE / sinks      │
└──────────────────────────────────────────────────────────┘
                          │ QUIC streams (multiplexed)
┌──────────────────────────────────────────────────────────┐
│  companion (Kotlin + Rust cdylib via JNI)                │
│  ├── MediaProjection capture → H.264                     │
│  ├── Camera2 → H.264 / H.265                             │
│  ├── AudioRecord / AudioTrack                            │
│  ├── ClipboardManager bridge                             │
│  ├── AccessibilityService (gesture replay)               │
│  └── SAF FS server                                       │
└──────────────────────────────────────────────────────────┘
```

Stream kinds: `Control`, `Video`, `Audio`, `Files`, `Fs`, `Input`, `Camera`, `Clipboard`. Cada uno es una QUIC bidi stream con un byte de tag al inicio. Opener escribe, accepter lee.

Todos los backends (audio, cámara, input, transporte, descubrimiento, FS, clipboard) están detrás de traits para sumar implementaciones (ALSA, JACK, PipeWire-camera, BT-HID, relay NAT) sin tocar el resto.

Codecs vía [ferricast](../../ferricast) — NVENC, VAAPI, openh264 SW fallback. **Cero ffmpeg, cero OpenSSL.**

## Install (NixOS)

```nix
{
  inputs.ansync.url = "github:SergioRibera/ansync";

  outputs = { self, nixpkgs, ansync, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      modules = [
        ansync.nixosModules.default
        {
          services.ansync = {
            enable = true;
            user = "alice";
          };
        }
      ];
    };
  };
}
```

El módulo carga uinput + v4l2loopback + FUSE, agrega `alice` a los grupos `input`/`video`/`fuse`, e instala el systemd user unit (`systemctl --user enable ansyncd`).

home-manager (sin NixOS):

```nix
home.imports = [ ansync.homeManagerModules.default ];
programs.ansync.enable = true;
```

## Install (manual)

Requisitos: PipeWire (o ALSA), v4l2loopback, FUSE3, BlueZ, D-Bus, wl-clipboard.

```sh
nix develop
cargo build --release
sudo install -Dm755 target/release/ansyncd /usr/local/bin/ansyncd
sudo install -Dm755 target/release/ansyncctl /usr/local/bin/ansyncctl
sudo install -Dm644 bins/ansyncd/contrib/60-ansync-uinput.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo modprobe v4l2loopback devices=1 video_nr=10 card_label="Ansync" exclusive_caps=1
sudo usermod -aG input,video,fuse $USER
```

## Pair

Conectá el Android vía USB con depuración habilitada. Una sola sesión:

```sh
ansyncctl pair
```

Lo que pasa:
1. `ansyncctl` descubre el device via `adb_client` (sin shell-out al binario `adb`).
2. Chequea si `org.gameros.ansync` está instalado. Si falta, fetcha el APK más reciente desde `github.com/SergioRibera/ansync/releases/latest` (cache en `$XDG_CACHE_HOME/ansync/`) y lo instala con `-r -g`.
3. Configura `adb reverse tcp:N tcp:N`, manda un broadcast `org.gameros.ansync.action.PAIR` para despertar al companion sin abrir la app.
4. Bootstrap Ed25519 sobre el cable, persistido en `$XDG_DATA_HOME/ansync/peers/{device_id}.toml`.
5. Si el daemon está corriendo, le pega un `Manager.RefreshPeers` D-Bus para que registre el nuevo Device sin restart.

Después del pair, en el companion aparece "Connect to {hostname} ({IP})" cuando el daemon está visible vía mDNS.

## D-Bus surface

```
Service: org.gameros.Ansync1

/org/gameros/Ansync1/Manager
  ListDevices() → a(s)
  ForgetDevice(id: s)
  RefreshPeers()
  → Signals: DeviceAdded(id) / DeviceRemoved(id)

/org/gameros/Ansync1/Device/{id}
  Props (RO): Id, Name, State, Capabilities, BatteryLevel, Address
  ShowScreen() / HideScreen()
  StartCamera(camera_id, w, h, fps, bitrate_kbps, codec, aspect, stabilization)
  StopCamera()
  StartMicrophone() / StopMicrophone()
  StartAudioRoute(direction) / StopAudioRoute()
  SyncClipboard()
  SendFile(path) / Mount(path) / Unmount()

/org/gameros/Ansync1/Permissions/{id}
  Get(flag) / Set(flag, value) / Reset()
  → Signal: PermissionChanged(flag, value)
```

Ejemplo: levantar mirror window y empujar la cámara trasera del peer al device virtual `/dev/video10`:

```sh
DEV_ID=$(gdbus call --session \
  --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Manager \
  --method org.gameros.Ansync1.Manager.ListDevices | grep -oE '[0-9a-f]{32}' | head -1)

gdbus call --session --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Device/$DEV_ID \
  --method org.gameros.Ansync1.Device.ShowScreen

gdbus call --session --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Device/$DEV_ID \
  --method org.gameros.Ansync1.Device.StartCamera \
  "0" 1280 720 30 4000 "h264" "letterbox" false
```

## Permisos por device

Flags en `$XDG_CONFIG_HOME/ansync/devices/{id}.toml`:

```
screen_mirror     camera_video      camera_audio      mic
audio_in          audio_out         files_send        files_receive
files_mount       clipboard_in      clipboard_out     input_from_device
input_to_device   notifications     sensors
```

Cada acción del daemon chequea el flag antes de proceder. Defaults al pairing: `screen_mirror`, `files_send`, `files_receive`, `notifications` **on**; `clipboard_*` **prompt**; resto **off**.

Toggle vía `ansyncctl perm <id> <flag> on|off` o D-Bus `Permissions.Set`.

## Troubleshooting

- `ansyncd` se queja de `BackendUnavailable` para camera → `v4l2loopback` no cargado o `card_label` no matchea. `lsmod | grep v4l2loopback` y `modprobe v4l2loopback ...`.
- Mirror window vacía → companion no abrió `StreamKind::Video` aún. Botón "Start screen capture" en el app, grant MediaProjection.
- Pair falla con "companion did not connect in time" → el broadcast `am broadcast PAIR` no llegó al `PairingReceiver`. `adb shell pm list packages org.gameros.ansync` para verificar install. El log del companion sale en `adb logcat -s ansync*`.
- Audio mudo → `pactl list short sinks` debería mostrar la default donde cpal escribe. Para route inverso, el RECORD_AUDIO runtime perm tiene que estar grant-ed en el companion.
- FUSE mount vacío → companion no eligió tree URI. "Pick shared folder" en MainActivity + `ACTION_OPEN_DOCUMENT_TREE`.

## Logs

```sh
# host
journalctl --user -u ansyncd -f

# companion
adb logcat -s ansync ansync.svc ansync.camera ansync.audio ansync.capture ansync.clip
```

## Licencia

MIT OR Apache-2.0
