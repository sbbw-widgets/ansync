# Consolidated NixOS module for ansync.
#
# Imports the per-feature partials (uinput / v4l2loopback) and wires
# them with the systemd user unit + udev rule + group memberships.
# Single import surface for users:
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
    ./v4l2loopback.nix
  ];

  options.services.ansync = {
    enable = lib.mkEnableOption "ansync — Android ↔ Linux integration daemon";

    user = lib.mkOption {
      type = lib.types.str;
      description = ''
        User the daemon runs as. The module adds this user to the
        `input` and `video` groups so the daemon can claim uinput
        and v4l2loopback without privilege escalation.
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

    quicPort = lib.mkOption {
      type = lib.types.port;
      default = 47215;
      description = ''
        UDP port the QUIC server binds to. The default matches
        `DaemonConfig.listen_addr` in the daemon — only override
        if you also pass `--listen 0.0.0.0:<port>` to ansyncd.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Open the firewall for the QUIC server port and mDNS (5353).
        Without this, the companion's connection attempts and the
        peer-discovery announce/browse both die at the kernel without
        reaching the daemon. Disable only if you manage firewall
        rules in a separate module.
      '';
    };

    downloadDir = lib.mkOption {
      type = lib.types.str;
      default = "%h/Downloads/ansync";
      description = ''
        Directory where files received from Android are saved.
        Supports systemd specifiers (%h = home dir). Must be
        writable by the daemon user.
      '';
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ ansyncPkg ];

    # User must be in the groups that own the relevant device nodes.
    users.users.${cfg.user}.extraGroups = [
      "input"
      "video"
    ] ++ cfg.extraGroups;

    # Install the udev rule + companion APK directory.
    services.udev.packages = [ ansyncPkg ];

    # Open the QUIC server port + mDNS so the companion can reach
    # the daemon and resolve its announce. Without 47215 the inbound
    # QUIC INITIAL is dropped and the device stays Disconnected
    # forever from the GUI's perspective.
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedUDPPorts = [ cfg.quicPort 5353 ];
    };

    # The daemon's systemd user unit ships inside the package; this
    # tells systemd to expose it so users can `systemctl --user
    # enable ansyncd`.
    systemd.user.services.ansyncd = {
      description = "ansync daemon (Android ↔ Linux integration)";
      wantedBy = [ "default.target" ];
      after = [ "graphical-session.target" ];

      # Cap the restart loop so a persistent misconfiguration (missing
      # XDG dirs, permission denial, corrupted identity.key) doesn't
      # thrash the process manager — systemd will stop the unit after
      # 5 restarts inside 30s and require a manual `systemctl --user
      # reset-failed ansyncd` to try again.
      startLimitBurst = 5;
      startLimitIntervalSec = 30;

      serviceConfig = {
        Type = "simple";
        ExecStart = "${ansyncPkg}/bin/ansyncd --download-dir ${cfg.downloadDir}";
        Restart = "on-failure";
        RestartSec = 2;

        # systemd creates `%t/ansync` (`/run/user/<uid>/ansync`) with
        # mode 0700 before the sandbox is built and adds it to the
        # implicit ReadWritePaths, so the mount-namespace setup no
        # longer fails with `/run/user/1000/ansync: No such file or
        # directory` (status=226/NAMESPACE).
        RuntimeDirectory = "ansync";

        # Same sandboxing knobs the standalone unit uses.
        NoNewPrivileges = true;
        ProtectSystem = "strict";

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
