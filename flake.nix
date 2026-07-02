{
  description = "ansync — Android ↔ Linux integration daemon (D-Bus, QUIC, mDNS)";

  inputs = {
    # Pinned to the same revision as the user's system flake.lock so the
    # store paths are already cached locally — `nix develop` does not
    # re-download nixpkgs.
    nixpkgs.url = "github:NixOS/nixpkgs/549bd84d6279f9852cae6225e372cc67fb91a4c1";

    flake-parts.url = "github:hercules-ci/flake-parts/0678d8986be1661af6bb555f3489f2fdfc31f6ff";

    crane.url = "github:ipetkov/crane/6d015ea29630b7ad2402841386da2cb617a470a7";

    rust-overlay = {
      url = "github:oxalica/rust-overlay/4852a8aa041c94af55e136cde5b8b6d42c3563e8";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    nix-bundle-app = {
      url = "github:SergioRibera/nix-bundle-app";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = inputs @ { flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [ "x86_64-linux" "aarch64-linux" ];

      flake = {
        nixosModules.default = ./nix/module.nix;
        homeManagerModules.default = ./nix/hm-module.nix;
      };

      perSystem = { system, lib, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            config.allowUnfree = true;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

          ansyncPkg = pkgs.callPackage ./nix/package.nix {
            inherit rustToolchain;
            crane = inputs.crane;
          };

          # Unwrapped variant for the bundler: no `wrapProgram` (the
          # bundler patchelfs RPATH itself), no contrib install (the
          # bundler stages those via `info.extraFiles`).
          ansyncPkgPortable = pkgs.callPackage ./nix/package.nix {
            inherit rustToolchain;
            crane = inputs.crane;
            portable = true;
          };

          bundler = inputs.nix-bundle-app.lib.mkLib pkgs;

          # System-wide systemd user unit shipped via extraFiles. The
          # in-tree `contrib/ansyncd.service` uses `%h/.cargo/bin/ansyncd`
          # which only makes sense for `cargo install`; bundler-installed
          # binaries land at `/usr/bin/ansyncd` (symlink to /opt/ansync).
          ansyncdSystemUnit = ''
            [Unit]
            Description=ansync daemon (Android ↔ Linux integration)
            Documentation=https://github.com/SergioRibera/ansync
            After=dbus.socket pipewire.socket
            Wants=dbus.socket

            [Service]
            Type=simple
            ExecStart=/usr/bin/ansyncd
            Restart=on-failure
            RestartSec=3
            Environment=RUST_LOG=info
            StandardOutput=journal
            StandardError=journal
            ProtectSystem=strict
            ProtectHome=read-only
            PrivateTmp=true
            NoNewPrivileges=true
            RuntimeDirectory=ansync
            ReadWritePaths=-%h/.local/share/ansync -%h/.config/ansync -%h/.cache/ansync

            [Install]
            WantedBy=default.target
          '';

          bundleInfo = {
            name = "ansync";
            version = ansyncPkg.version;
            summary = "Android ↔ Linux integration daemon (mirror, input, files, camera, audio, clipboard)";
            longDescription = ''
              ansync is a Rust rewrite of scrcpy with extended scope: screen
              mirroring, bidirectional input, file transfer, virtual camera
              + microphone, bidirectional audio, clipboard sync, mDNS discovery,
              Ed25519 pairing over QUIC. Daemon (`ansyncd`) + CLI (`ansyncctl`).
            '';
            license = "MIT";
            maintainer = "Sergio Ribera <sergioalejandroriberacosta@gmail.com>";
            homepage = "https://github.com/SergioRibera/ansync";
            bundleId = "com.sergioribera.ansync";

            # `autoDepends` scans the staged ELFs and resolves SONAMEs
            # against the curated lib-map. Manual lists below cover what
            # the scanner misses (kernel modules, runtime tools).
            depends = {
              deb = [ "v4l2loopback-dkms" ];
              rpm = [ "v4l2loopback" ];
              archlinux = [ "v4l2loopback-dkms" ];
            };

            # Daemon-only: no desktop entries.
            desktopEntries = [ ];

            # System-wide user units + kernel module config + udev rules.
            # `/usr/lib/systemd/user/` is the per-package user-unit dir
            # systemd auto-discovers; users `systemctl --user enable
            # ansyncd` post-install.
            extraFiles = {
              "/usr/lib/systemd/user/ansyncd.service" = ansyncdSystemUnit;
              "/lib/udev/rules.d/60-ansync-uinput.rules" =
                ./bins/ansyncd/contrib/60-ansync-uinput.rules;
              "/lib/udev/rules.d/61-ansync-v4l2loopback.rules" =
                ./bins/ansyncd/contrib/61-ansync-v4l2loopback.rules;
              "/etc/modprobe.d/ansync-v4l2loopback.conf" =
                ./bins/ansyncd/contrib/ansync-v4l2loopback.conf;
              "/etc/modules-load.d/ansync.conf" =
                ./bins/ansyncd/contrib/ansync-modules-load.conf;
            };

            flatpak = {
              # Assume host has v4l2loopback preloaded (the module + its
              # /dev/video* nodes live outside the sandbox; sharing the
              # device node into the flatpak is the user's call).
              finishArgs = [
                "--share=network"
                "--share=ipc"
                "--socket=wayland"
                "--socket=pulseaudio"
                "--device=all"
                "--filesystem=xdg-config/ansync:create"
                "--filesystem=xdg-data/ansync:create"
                "--filesystem=xdg-cache/ansync:create"
                "--filesystem=xdg-download"
                "--talk-name=org.freedesktop.DBus"
                "--system-talk-name=org.freedesktop.Avahi"
              ];
            };
          };

          bundleFormats = [
            "deb"
            "rpm"
            "archlinux"
            # "appimage" — disabled 2026-06-24: upstream type2-runtime
            # rebuilt the `continuous` release with a new SHA, and
            # nix-bundle-app pins the old hash, so every CI build hard-
            # fails the fixed-output derivation. Re-enable once the
            # nix-bundle-app input bumps to a runtime hash that matches
            # (or pins a stable type2-runtime tag instead of `continuous`).
            "flatpak"
            "tar.zst"
          ];

          # Native build deps required at link / build time.
          nativeBuildDeps = with pkgs; [
            pkg-config
            cmake
            clang
            rustToolchain
          ];

          # Runtime / FFI deps. Provided to dev shell so cargo can link
          # against them when individual crate features get enabled.
          runtimeDeps = with pkgs; [
            # IPC
            dbus

            # Audio
            pipewire
            alsa-lib
            libopus

            # Video (Steps 5 / 6)
            libva
            libva-utils
            cudaPackages.cuda_cudart

            # Camera
            v4l-utils

            # Gamepad input — `gilrs` opens evdev nodes for connected
            # controllers and pulls hot-plug events from libudev. We
            # use `eudev` (the systemd-less fork) so the runtime
            # closure doesn't drag in the full init system.
            eudev

            # GUI — wgpu + eframe runtime
            wayland
            libGL
            libxkbcommon
            vulkan-loader
          ];
        in
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = nativeBuildDeps;
            buildInputs = runtimeDeps;

            # `/run/opengl-driver/lib` ships the proprietary NVIDIA
            # runtime libs on NixOS hosts with `hardware.nvidia.*`
            # enabled — `libcuda.so.1` (required by NVDEC / NVENC) and
            # `libnvidia-encode.so.1` among them. It MUST come first
            # so the real driver shadows our nixpkgs `cuda_cudart`
            # stub, otherwise ferricast's NVDEC backend falls back to
            # openh264 (software) at runtime.
            LD_LIBRARY_PATH = "/run/opengl-driver/lib:${lib.makeLibraryPath runtimeDeps}";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

            BINDGEN_EXTRA_CLANG_ARGS =
              "-isystem ${pkgs.glibc.dev}/include -I${pkgs.libclang.lib}/lib/clang/${pkgs.clang.version}/include";
          };

          packages = {
            default = ansyncPkg;
            ansync = ansyncPkg;
            ansync-portable = ansyncPkgPortable;

            # All distro bundles in one drv (`result/` ends up with
            # `.deb`, `.rpm`, `.pkg.tar.zst`, `.AppImage`, flatpak src
            # tree, `tar.zst`).
            bundle-all = bundler.bundleAll {
              drv = ansyncPkgPortable;
              formats = bundleFormats;
              info = bundleInfo;
            };

            # Release matrix: every format per supported arch + install
            # scripts + SHA256SUMS. release.yml uploads the contents
            # verbatim to the GitHub Release.
            release = bundler.release {
              info = bundleInfo;
              releaseUrl = "https://github.com/SergioRibera/ansync/releases/download/v\${VERSION}";

              matrix = {
                "x86_64-linux" = {
                  drv = ansyncPkgPortable;
                  formats = bundleFormats;
                };
              };

              installScripts = true;
            };
          };

          apps = {
            ansyncd = {
              type = "app";
              program = "${ansyncPkg}/bin/ansyncd";
            };
            ansyncctl = {
              type = "app";
              program = "${ansyncPkg}/bin/ansyncctl";
            };
          };
        };
    };
}
