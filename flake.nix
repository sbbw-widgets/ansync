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

            # Clipboard
            wl-clipboard
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
