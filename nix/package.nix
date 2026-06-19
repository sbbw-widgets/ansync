# crane-based build derivation for the ansync host binaries.
#
# Builds `ansyncd` (daemon + GUI) and `ansyncctl` (CLI) from the
# workspace. Reuses crane's incremental cache so re-builds of the
# bin only recompile the bin layer.
#
# Step 14 wires this into `flake.nix` outputs.
{ pkgs
, crane
, rustToolchain
, lib ? pkgs.lib
}:

let
  craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

  src = lib.cleanSourceWith {
    src = ./..;
    filter = path: type:
      # Keep Cargo metadata + every Rust source + any non-Rust assets the
      # build scripts read at compile time. Drop docs / android sources /
      # any build artefacts so the derivation hash doesn't churn on doc
      # edits.
      (craneLib.filterCargoSources path type)
      || builtins.match ".*\\.(toml|lock|rules|service|target)$" (toString path) != null;
    name = "ansync-source";
  };

  commonArgs = {
    inherit src;
    pname = "ansync";
    version = "0.1.0";
    strictDeps = true;

    nativeBuildInputs = with pkgs; [
      pkg-config
      cmake
      clang
      makeWrapper
    ];

    buildInputs = with pkgs; [
      dbus
      pipewire
      alsa-lib
      libva
      libva-utils
      cudaPackages.cuda_cudart
      v4l-utils
      wayland
      libGL
      libxkbcommon
      udev
      vulkan-loader
      wl-clipboard
    ];

    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
  };

  cargoArtifacts = craneLib.buildDepsOnly commonArgs;

  # Runtime libs the wrapped binary must dlopen — CUDA + NVENC come
  # from `/run/opengl-driver/lib` on NixOS hosts with the proprietary
  # NVIDIA driver, which MUST shadow our `cuda_cudart` stub.
  runtimeLibs = with pkgs; [
    libva
    libGL
    vulkan-loader
    wayland
    libxkbcommon
    cudaPackages.cuda_cudart
  ];
in
craneLib.buildPackage (commonArgs // {
  inherit cargoArtifacts;
  doCheck = false;
  cargoExtraArgs = "--workspace --bins";

  # Embedded udev rule + systemd unit ship next to the binaries so
  # the NixOS module can install them with `pkg.passthru.contrib`.
  # Wrap `ansyncd` so NVDEC / NVENC find the host's NVIDIA runtime;
  # without this the daemon silently falls back to openh264 (software).
  postInstall = ''
    install -Dm0644 bins/ansyncd/contrib/60-ansync-uinput.rules \
      "$out/lib/udev/rules.d/60-ansync-uinput.rules"
    install -Dm0644 bins/ansyncd/contrib/ansyncd.service \
      "$out/lib/systemd/user/ansyncd.service"

    wrapProgram "$out/bin/ansyncd" \
      --prefix LD_LIBRARY_PATH : "/run/opengl-driver/lib:${lib.makeLibraryPath runtimeLibs}"
  '';

  meta = with lib; {
    description = "ansync daemon + CLI (Android ↔ Linux integration)";
    homepage = "https://github.com/SergioRibera/ansync";
    license = licenses.mit;
    maintainers = [ ];
    mainProgram = "ansyncd";
  };
})
