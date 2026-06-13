# Standalone NixOS module fragment that prepares the host for
# ansync's virtual camera backend.
#
# Wires three things:
#   1. The `v4l2loopback` kernel module added to extraModulePackages
#      so it actually compiles against the running kernel (it's an
#      out-of-tree module).
#   2. modprobe options that create one device at boot named
#      "Ansync" with exclusive_caps=1 (so apps see a regular V4L2
#      capture device, not a hybrid output/capture node — Chromium /
#      OBS / Firefox all require this).
#   3. A udev rule that makes the resulting /dev/videoN node group-
#      owned by `video` (the ansyncd user joins this group; Step 14
#      wires the explicit user-group bridge).
#
# Card label = "Ansync" is what shows up in browser camera pickers,
# Discord, OBS. The peer's nice name surfaces in tracing logs +
# the D-Bus Device.Name property; the kernel-level card_label is
# fixed at module load time so we can't substitute it per-peer.
#
# Step 14 imports this fragment into the consolidated NixOS module
# so end users get a fully plug-and-play install via the flake.
{ config, lib, pkgs, ... }:

{
  boot.extraModulePackages = [ config.boot.kernelPackages.v4l2loopback ];
  boot.kernelModules = [ "v4l2loopback" ];

  boot.extraModprobeConfig = ''
    options v4l2loopback devices=1 video_nr=10 card_label="Ansync" exclusive_caps=1
  '';

  # Make the loopback node accessible to the `video` group rather
  # than just root. Mirrors how /dev/video0 is set up by upstream
  # v4l-utils.
  services.udev.extraRules = ''
    KERNEL=="video[0-9]*", ATTR{name}=="Ansync*", GROUP="video", MODE="0660"
  '';

  # Until Step 14 wires the consolidated module, users have to add
  # themselves to `video` explicitly:
  #   users.users.<name>.extraGroups = [ "video" ];
}
