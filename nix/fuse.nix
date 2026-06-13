# NixOS module fragment that prepares the host for the ansync FUSE
# mount.
#
# Three things:
#   1. Ensure `fuse3` userspace + kernel module are present.
#   2. Set `user_allow_other = yes` in `/etc/fuse.conf` so the
#      `AllowOther` mount option (used so other users can read the
#      mount; required for per-app sandboxed access) works.
#   3. Add the daemon's user to the `fuse` group so it can call
#      `fusermount3` without setuid root.
#
# Step 14 imports this fragment into the consolidated NixOS module.
{ config, lib, pkgs, ... }:

{
  environment.systemPackages = [ pkgs.fuse3 ];

  boot.kernelModules = [ "fuse" ];

  programs.fuse.userAllowOther = true;

  # Callers still need to add their user to `fuse` explicitly until
  # the home-manager module lands in Step 14:
  #   users.users.<name>.extraGroups = [ "fuse" ];
}
