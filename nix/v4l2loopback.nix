# Standalone NixOS module fragment that prepares the host for
# ansync's virtual camera backend.
#
# Wires four things:
#   1. The `v4l2loopback` kernel module added to extraModulePackages
#      so it actually compiles against the running kernel (it's an
#      out-of-tree module).
#   2. modprobe options that load the module in DYNAMIC mode
#      (devices=0). ansyncd talks to `/dev/v4l2loopback` and creates
#      one `/dev/videoN` per connected peer, with the peer's
#      Android device name as the card_label. The legacy
#      `devices=1 video_nr=10 card_label="Ansync"` static path is
#      retired — it forced every peer to share the same name in
#      browser camera pickers.
#   3. A udev rule that exposes the `/dev/v4l2loopback` control
#      device to the `video` group so the daemon can ADD/REMOVE
#      nodes without root.
#   4. A udev rule that makes the dynamically-created /dev/videoN
#      nodes group-owned by `video` (the ansyncd user joins this
#      group via the consolidated module in Step 14). Matches any
#      v4l2loopback-driven device — the per-peer label varies so we
#      can't ATTR-match on the name here.
#
# Card label per peer = "<Android model> (Ansync)" surfaces in
# Chromium / Firefox / OBS / Discord pickers, so users with multiple
# paired devices can pick the right one without a guessing game.
#
# Step 14 imports this fragment into the consolidated NixOS module
# so end users get a fully plug-and-play install via the flake.
{ config, lib, pkgs, ... }:

{
  boot.extraModulePackages = [ config.boot.kernelPackages.v4l2loopback ];
  boot.kernelModules = [ "v4l2loopback" ];

  # devices=0 ⇒ load the module without pre-creating any /dev/videoN
  # nodes. ansyncd uses the V4L2LOOPBACK_CTL_ADD ioctl on
  # /dev/v4l2loopback to allocate one per peer at connect time.
  # exclusive_caps=1 stays in the per-ADD ioctl payload (default).
  boot.extraModprobeConfig = ''
    options v4l2loopback devices=0
  '';

  # Control device + dynamically created video nodes both go to the
  # `video` group. The first rule covers the control char dev that
  # only exists with recent v4l2loopback (0.13+). The second rule is
  # a catch-all for any v4l2 device whose driver reports as
  # v4l2loopback — covers every node we ADD via ioctl regardless of
  # the per-peer card_label.
  services.udev.extraRules = ''
    KERNEL=="v4l2loopback", GROUP="video", MODE="0660"
    SUBSYSTEM=="video4linux", ATTRS{name}=="v4l2 loopback*", GROUP="video", MODE="0660"
    SUBSYSTEM=="video4linux", ATTR{name}=="*(Ansync)*", GROUP="video", MODE="0660"
  '';

  # Until Step 14 wires the consolidated module, users have to add
  # themselves to `video` explicitly:
  #   users.users.<name>.extraGroups = [ "video" ];
}
