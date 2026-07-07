# ansync

**Your Android phone, fully connected to your Linux desktop.**

Mirror the screen. Send files in either direction. Use the phone's camera as
a webcam in Zoom, OBS, or Discord. Type with your PC keyboard on the phone.
Copy from one, paste on the other. Open a link on one, watch it open on the
other. All over Wi-Fi, no cables once paired.

It's what scrcpy could be if it grew up.

https://github.com/user-attachments/assets/7722df7e-6e8f-4c5f-bea3-2a899e8a4062

---

## What you can do with it

- **Mirror your phone screen** to a window on your desktop.
- **Click, type, and drag** on that window — the phone responds.
- **Use the phone's camera as a webcam** in any Linux app that picks a v4l2
  camera (Chromium-based browsers, Firefox, OBS Studio, Discord, Zoom, Google
  Meet, ...). The phone shows up by its actual model name in the camera picker.
- **Use the phone's microphone** as a Linux audio source.
- **Hear PC audio on the phone** (and vice versa) over the same Wi-Fi link.
- **Send a file from anywhere on Linux** to the phone (`ansyncctl push file.pdf`)
  or via a desktop file-manager menu.
- **Share files from the phone to the PC** using Android's built-in share
  sheet — pick "Send via ansync".
- **Share a link in either direction**: it opens automatically on the other
  side (the phone asks first, the PC opens it straight away).
- **Sync the clipboard** both ways: copy on the phone, paste on the PC.
- **See phone notifications** on the desktop, mirrored in real time.

Everything is end-to-end encrypted between your phone and your computer.
No cloud, no Anthropic account, no Google account — once paired, the two
devices find each other on your home Wi-Fi and talk directly.

---

## Install

### NixOS (recommended)

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
            user = "alice";          # whichever user runs the desktop
          };
        }
      ];
    };
  };
}
```

Rebuild your system, log out and back in. The daemon (`ansyncd`) starts
automatically with your desktop session.

Home Manager (non-NixOS distros that already use Home Manager):

```nix
home.imports = [ ansync.homeManagerModules.default ];
programs.ansync.enable = true;
```

### Other Linux distros

The daemon ships as a single binary. You'll need a few things from your
package manager first:

- PipeWire (or ALSA)
- `v4l2loopback` kernel module
- `wl-clipboard` (Wayland) or `xclip` (X11)
- `xdg-utils` (so links open via `xdg-open`)

Then:

```sh
nix develop           # or set up Rust 1.85+ and pkg-config / libclang manually
cargo build --release
sudo install -Dm755 target/release/ansyncd /usr/local/bin/ansyncd
sudo install -Dm755 target/release/ansyncctl /usr/local/bin/ansyncctl
sudo install -Dm644 bins/ansyncd/contrib/60-ansync-uinput.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo modprobe v4l2loopback devices=0
sudo usermod -aG input,video $USER     # log out / log back in
```

Make sure your firewall lets UDP `5353` (mDNS) and UDP `47215` (the daemon's
encrypted port) through:

```sh
# firewalld
sudo firewall-cmd --add-port=47215/udp --permanent && sudo firewall-cmd --reload
# ufw
sudo ufw allow 47215/udp
```

If you used the NixOS module, the firewall is already open.

### Android companion app

You install the companion app **once, from your PC**, the first time you
pair. There is no need to download an APK by hand — see the next section.

---

## First pair (one-time setup)

1. On the phone: enable **Developer Options** (tap the build number 7 times
   in Settings) and turn on **USB Debugging**.
2. Plug the phone into the PC with a USB cable.
3. On the PC, run:

   ```sh
   ansyncctl pair
   ```

That's it. Behind the scenes, ansync downloads the latest companion app, installs it, exchanges encryption keys, grants the required permissions, and registers the phone with the daemon. The whole thing takes about 15 seconds.

After this, **unplug the cable**. The two devices find each other on your
Wi-Fi from now on.

You'll see a setup notification on the phone walking you through one-time
grants (screen capture access, accessibility for remote input,
notification access). Tap through them once and they're remembered.

---

## How to use it

Once the phone is paired and on the same Wi-Fi:

### Mirror the phone's screen

From your PC:

```sh
# Find the device id (32 hex characters)
ansyncctl devices

# Open the mirror window
gdbus call --session --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Device/<ID> \
  --method org.gameros.Ansync1.Device.ShowScreen
```

A window pops up with the phone's screen. Click, drag, type — the phone
responds in real time. Close the window to stop.

(A friendly GUI / system-tray launcher is planned — for now, the GNOME
extension and KDE plasmoid in the works are the easy path; the
`gdbus` invocation is the underlying mechanism.)

### Use the phone as a webcam

```sh
gdbus call --session --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Device/<ID> \
  --method org.gameros.Ansync1.Device.StartCamera \
  "0" 1920 1080 30 6000 "h264" "letterbox" false
```

Now open Chromium, Firefox, OBS, Discord, Meet, ... and pick the camera
**`<Your Phone> (Ansync)`** in the device list. Done.

Lens id `"0"` is the rear camera on most phones; `"1"` is the front. The
five numbers are `width height fps bitrate-kbps`, then codec
(`h264` or `h265`), aspect (`crop` / `letterbox` / `stretch`), and whether
to enable image stabilisation (`true` / `false`).

Stop with `StopCamera`.

### Use the phone's microphone

```sh
gdbus call --session --dest org.gameros.Ansync1 \
  --object-path /org/gameros/Ansync1/Device/<ID> \
  --method org.gameros.Ansync1.Device.StartMicrophone
```

A new PipeWire source appears: pick it in your audio settings. Stop with
`StopMicrophone`.

### Send files PC → phone

```sh
ansyncctl push <device-id> ~/Documents/notes.pdf ~/Pictures/cat.png
```

The phone gets a notification you can tap to open the file.

### Send files phone → PC

On the phone, open any file (a photo, a PDF, anything), tap the
**Share** button, and choose **Send via ansync** from the share sheet.
A desktop notification appears on the PC when it arrives.

You can also tap the **Send to PC** Quick Settings tile on the phone to
pick any file without opening a specific app.

### Send a link in either direction

PC → phone:

```sh
ansyncctl url <device-id> "https://example.com"
```

The phone shows a "Open this link from your PC?" notification — tap and
the link opens.

Phone → PC: just share the link from any app via **Send via ansync**. It
opens immediately on your desktop (the phone is already trusted).

### Clipboard, notifications

Both sync automatically once you're paired. You can tighten or loosen what
gets shared per-device under **Permissions** (see below).

---

## Permissions

Every feature is gated per device. The first time you pair, the safe
defaults are on: screen mirror, file transfer, notifications, clipboard.
Things that grant access to hardware (camera, microphone, virtual
keyboard / mouse) start **off** — flip them on explicitly.

```sh
ansyncctl perm <device-id> camera_video on
ansyncctl perm <device-id> mic on
```

Or edit `~/.config/ansync/devices/<device-id>.toml` directly.

Available switches:

| Switch              | What it controls |
|---------------------|------------------|
| `screen_mirror`     | Mirror the phone's screen on the PC |
| `camera_video`      | Use the phone's camera as a webcam |
| `camera_audio`      | Audio track from the phone's camera (with video) |
| `mic`               | Use the phone's microphone on the PC |
| `audio_in`          | Phone → PC audio (e.g. play phone audio on PC) |
| `audio_out`         | PC → phone audio (e.g. play music on phone) |
| `files_send`        | PC sends files to phone |
| `files_receive`     | PC receives files from phone |
| `clipboard_in`      | Phone clipboard appears on PC |
| `clipboard_out`     | PC clipboard appears on phone |
| `input_from_device` | Phone can control the PC's keyboard / mouse |
| `input_to_device`   | PC can control the phone (mirror clicks) |
| `notifications`     | Phone notifications appear on PC |
| `share_receive`     | Accept shared files / links from the phone |

---

## Troubleshooting

**The phone never connects** ("Disconnected" forever, even after pair).
Make sure UDP `47215` is open on your PC's firewall. On NixOS with the
module enabled this is automatic.

**Camera picker doesn't show "(Ansync)"**. Check `lsmod | grep v4l2loopback`.
If you see the module loaded with `devices=N video_nr=...`, reload it with
`devices=0`: `sudo modprobe -r v4l2loopback && sudo modprobe v4l2loopback devices=0`.
The dynamic mode gives every paired phone its own named entry.

**Microphone silent or no sound coming through**. Open Settings → Sound on
the phone, confirm RECORD_AUDIO is granted to the ansync companion. On the
PC, `pactl list short sinks` should list the ansync sink.

**Mirror window opens but is blank**. On the phone, tap the "Mirroring to
PC" tile in Quick Settings (or pull down the shade and grant screen capture
from the heads-up notification).

**Pair fails with "companion did not connect in time"**. Make sure USB
debugging is enabled and the cable is data-capable. Some charge-only
cables look fine but don't carry data.

**Logs**

```sh
journalctl --user -u ansyncd -f                                 # PC daemon
adb logcat -s ansync ansync.svc ansync.camera ansync.audio       # phone
```

---

## How it works (one-paragraph version)

The PC runs a small daemon (`ansyncd`) that listens for the phone on the
local network. The phone runs a background service that finds the daemon
over mDNS and connects with QUIC (the same encrypted transport HTTP/3
uses). The keys exchanged when you paired are pinned on both ends, so
nothing but those two specific devices can decrypt the traffic. Every
feature — screen, camera, audio, files, clipboard — is a separate
multiplexed stream on that single QUIC connection.

No cloud. No accounts. No telemetry. No ffmpeg, no OpenSSL — codecs are
hardware NVENC / VAAPI with a pure-Rust software fallback, and crypto is
`rustls` with custom public-key pinning.

---

## For developers

The full design notes, roadmap, and per-feature breakdown live in
[`PLAN.md`](./PLAN.md). The session-to-session conventions for working on
the codebase live in [`CLAUDE.md`](./CLAUDE.md). The Android companion
app's build / layout notes are in [`android/README.md`](./android/README.md).

The codebase is a Rust workspace (`crates/*` for libraries, `bins/*` for
the daemon and CLI) plus a Kotlin Android app under `android/` that links
a Rust cdylib via JNI.

```sh
nix develop                # all build deps
cargo build --workspace
cargo test  --workspace
```

Pull requests welcome.

## Licence

MIT OR Apache-2.0
