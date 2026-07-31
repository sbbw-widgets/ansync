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
        UDP port the zudp data server binds to. The default matches
        `DaemonConfig.listen_addr` in the daemon — only override
        if you also pass `--listen 0.0.0.0:<port>` to ansyncd.
      '';
    };

    openFirewall = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = ''
        Open the firewall for the zudp data port and the zudp
        discovery port (7701). Without both, the companion cannot
        reach the daemon and Probe/Beacon discovery is silently
        dropped. Disable only if you manage firewall rules elsewhere.
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

    # Data port: inbound zudp connections from the companion.
    # Discovery port 7701: zudp Probe/Beacon LAN broadcast — companion
    # sends a Probe and the daemon responds with a Beacon carrying its
    # address + metadata. Without 7701 auto-discovery is silently dead.
    networking.firewall = lib.mkIf cfg.openFirewall {
      allowedUDPPorts = [ cfg.quicPort 7701 ];
    };

    # The daemon's systemd user unit ships inside the package; this
    # tells systemd to expose it so users can `systemctl --user
    # enable ansyncd`.
    systemd.user.services.ansyncd = {
      description = "ansync daemon (Android ↔ Linux integration)";
      wantedBy = [ "graphical-session.target" ];
      # network-online.target: zudp discovery broadcasts need a real LAN IP —
      # without it the Beacon response comes from the wrong interface and the
      # companion can't dial back. pipewire.socket: audio backend init at
      # startup; without it the first connection silently skips audio.
      after = [
        "graphical-session.target"
        "network-online.target"
        "pipewire.socket"
      ];
      wants = [ "network-online.target" "pipewire.socket" ];

      # Persistent misconfigurations (bad identity, missing XDG dirs) should
      # not thrash the process manager.  Transient failures (net not up yet,
      # pipewire slow start) are handled by the Restart policy below.
      startLimitBurst = 5;
      startLimitIntervalSec = 60;

      serviceConfig = {
        Type = "simple";
        ExecStart = "${ansyncPkg}/bin/ansyncd --download-dir ${cfg.downloadDir}";
        # on-failure + 3 s back-off covers the transient window where the
        # network or pipewire socket appears after the unit fires.
        Restart = "on-failure";
        RestartSec = 3;

        # systemd creates `%t/ansync` (`/run/user/<uid>/ansync`) with
        # mode 0700 before the sandbox is built and adds it to the
        # implicit ReadWritePaths, so the mount-namespace setup no
        # longer fails with `/run/user/1000/ansync: No such file or
        # directory` (status=226/NAMESPACE).
        RuntimeDirectory = "ansync";

        # Same sandboxing knobs the standalone unit uses.
        NoNewPrivileges = true;
        ProtectSystem = "strict";

        # zudp data + discovery listeners bind to LAN ports >1024;
        # no elevated capabilities needed.
        AmbientCapabilities = [ ];
        CapabilityBoundingSet = [ ];
        PrivateDevices = false;
        DevicePolicy = "auto";
      };
    };
  };
}
