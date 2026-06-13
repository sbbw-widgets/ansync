# Standalone NixOS module fragment that prepares the host for
# ansync's virtual input backend.
#
# Wires three things:
#   1. The `uinput` kernel module loaded at boot.
#   2. The 60-ansync-uinput.rules udev rule that exposes /dev/uinput
#      to anything in the `input` group (mode 0660).
#   3. A reminder to add the daemon's user to that group (handled by
#      the full module in `module.nix` once Step 14 lands; for now
#      callers do it manually).
#
# Step 14 imports this fragment into the consolidated NixOS module so
# end users get a fully plug-and-play install via the flake.
{ config, lib, pkgs, ... }:

{
  boot.kernelModules = [ "uinput" ];

  services.udev.packages = [
    (pkgs.runCommand "ansync-udev-rules" { } ''
      install -Dm0644 ${./../bins/ansyncd/contrib/60-ansync-uinput.rules} \
        $out/lib/udev/rules.d/60-ansync-uinput.rules
    '')
  ];

  # Until Step 14 wires the home-manager module, users have to add
  # themselves to `input` explicitly:
  #   users.users.<name>.extraGroups = [ "input" ];
}
