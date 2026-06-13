# Consolidated NixOS module for ansync.
#
# Imports the per-feature partials (uinput / fuse / v4l2loopback) and
# wires them with the systemd user unit + udev rule + group
# memberships. Single import surface for users:
#
#   imports = [ inputs.ansync.nixosModules.default ];
#   services.ansync = {
#     enable = true;
#     user = "alice";
#   };
{ config, lib, pkgs, ... }:

let
  cfg = config.services.ansync;
  ansyncPkg = cfg.package;
in
{
  imports = [
    ./uinput.nix
    ./fuse.nix
    ./v4l2loopback.nix
  ];

  options.services.ansync = {
    enable = lib.mkEnableOption "ansync — Android ↔ Linux integration daemon";

    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        User the daemon runs as. The module adds this user to the
        `input`, `video`, and `fuse` groups so the daemon can claim
        uinput, v4l2loopback, and FUSE mounts without privilege
        escalation.
      '';
    };

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.ansync or (pkgs.callPackage ./package.nix { });
      description = "ansync host package (built from `nix/package.nix`).";
    };

    extraGroups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      description = "Additional groups to add the daemon user to.";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ ansyncPkg ];

    # User must be in the groups that own the relevant device nodes.
    users.users.${cfg.user}.extraGroups = [
      "input"
      "video"
      "fuse"
    ] ++ cfg.extraGroups;

    # Install the udev rule + companion APK directory.
    services.udev.packages = [ ansyncPkg ];

    # The daemon's systemd user unit ships inside the package; this
    # tells systemd to expose it so users can `systemctl --user
    # enable ansyncd`.
    systemd.user.services.ansyncd = {
      description = "ansync daemon (Android ↔ Linux integration)";
      wantedBy = [ "default.target" ];
      after = [ "graphical-session.target" ];

      serviceConfig = {
        Type = "simple";
        ExecStart = "${ansyncPkg}/bin/ansyncd";
        Restart = "on-failure";
        RestartSec = 2;

        # Same sandboxing knobs the standalone unit uses.
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = "read-only";
        ReadWritePaths = [
          "%t/ansync"
          "%h/.config/ansync"
          "%h/.local/share/ansync"
          "%h/.cache/ansync"
        ];

        # The mDNS + QUIC listener bind to multicast + the LAN; the
        # daemon doesn't need privileged ports.
        AmbientCapabilities = [ ];
        CapabilityBoundingSet = [ ];
        PrivateDevices = false;
        DevicePolicy = "auto";
      };
    };
  };
}
