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

      perSystem = { system, lib, ... }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

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

            # Audio (Step 11)
            pipewire

            # Video (Steps 5 / 6)
            libva
            libva-utils

            # Camera (Step 10)
            v4l-utils

            # Filesystem (Step 9)
            fuse3

            # Bluetooth (Step 13)
            bluez

            # GUI (Step 6) — wgpu + eframe runtime
            wayland
            libxkbcommon
            vulkan-loader

            # Clipboard (Step 12)
            wl-clipboard
          ];
        in
        {
          devShells.default = pkgs.mkShell {
            nativeBuildInputs = nativeBuildDeps;
            buildInputs = runtimeDeps;

            shellHook = ''
              echo "ansync dev shell — rust $(rustc --version 2>/dev/null | awk '{print $2}')"
              echo "Run: cargo check --workspace"
            '';

            LD_LIBRARY_PATH = lib.makeLibraryPath runtimeDeps;

            # bindgen (used transitively by ferricast's VA-API and NVDEC
            # build scripts) needs libclang to parse system headers.
            # Without LIBCLANG_PATH it falls back to scanning /usr/lib
            # and panics inside a pure nix shell.
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
          };

          # Build derivations (ansyncd / ansyncctl) get wired in Step 14
          # using crane.mkLib pkgs |> .overrideToolchain rustToolchain.
        };
    };
}
