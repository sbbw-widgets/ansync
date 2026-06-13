# home-manager module for ansync.
#
# Mirrors the NixOS module but for users who want per-user install
# without root. The systemd user unit lives entirely in the user's
# home so groups + udev rules need to be set up externally (the
# NixOS module is the supported path for that — this exists for
# nix-darwin / non-NixOS Linux users who only want the binary +
# autostart).
{ config, lib, pkgs, ... }:

let
  cfg = config.programs.ansync;
in
{
  options.programs.ansync = {
    enable = lib.mkEnableOption "ansync host install (user-level)";

    package = lib.mkOption {
      type = lib.types.package;
      default = pkgs.ansync or (pkgs.callPackage ./package.nix { });
      description = "ansync host package.";
    };

    autoStart = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Bring the daemon up via a systemd user unit on login.";
    };
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    systemd.user.services.ansyncd = lib.mkIf cfg.autoStart {
      Unit = {
        Description = "ansync daemon (Android ↔ Linux integration)";
        After = [ "graphical-session.target" ];
      };
      Service = {
        ExecStart = "${cfg.package}/bin/ansyncd";
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "default.target" ];
    };
  };
}
