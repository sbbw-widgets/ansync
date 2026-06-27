# crane-based build derivation for the ansync host binaries.
#
# Builds `ansyncd` (daemon + GUI) and `ansyncctl` (CLI) from the
# workspace. Reuses crane's incremental cache so re-builds of the
# bin only recompile the bin layer.
#
# `portable = true` skips `wrapProgram` and the contrib install — the
# nix-bundle-app pipeline takes over (`patchelf` for RPATH, `extraFiles`
# for the udev rules + modprobe + modules-load fragments). The wrapped
# variant (`portable = false`, default) is what the NixOS module ships.
{ pkgs
, crane
, rustToolchain
, lib ? pkgs.lib
, portable ? false
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
      || builtins.match ".*\\.(toml|lock|rules|service|target|conf)$" (toString path) != null;
    name = "ansync-source";
  };

  # Pull the version straight from `Cargo.toml` so the derivation
  # metadata, bundle filenames, and the runtime `CARGO_PKG_VERSION` the
  # daemon embeds all stay in lockstep. Release tagging only needs to
  # bump the manifest — CI's "Verify Cargo.toml version matches tag"
  # step then guarantees tag == derivation == daemon.
  cargoToml = builtins.fromTOML (builtins.readFile ../Cargo.toml);

  commonArgs = {
    inherit src;
    pname = "ansync";
    version = cargoToml.workspace.package.version or cargoToml.package.version;
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
    ];

    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

    # bindgen-rs needs the system include paths (`stdint.h`, etc.) so
    # `clang_macro_fallback` can evaluate macros like
    # `#define SPA_ID_INVALID ((uint32_t)0xffffffff)`. Without these
    # the cast fails, bindgen silently drops the constant, and
    # libspa's `pub const ID_INVALID: u32 = spa_sys::SPA_ID_INVALID;`
    # fails with `not found in crate \`spa_sys\``. The clang resource
    # dir layout uses the major version only (`lib/clang/21/include`).
    # `-resource-dir` is what teaches clang where its own intrinsics
    # live; without it the bindgen-launched clang can't find
    # `__stddef_max_align_t.h` and falls back to system gcc headers
    # (which lack the macro guards bindgen relies on).
    BINDGEN_EXTRA_CLANG_ARGS =
      let
        clangMajor = lib.head (lib.splitString "." pkgs.clang.version);
        resourceDir = "${pkgs.libclang.lib}/lib/clang/${clangMajor}";
      in
      lib.concatStringsSep " " [
        "-resource-dir ${resourceDir}"
        "-isystem ${resourceDir}/include"
        "-isystem ${pkgs.glibc.dev}/include"
      ];
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
  # `portable` builds skip both (bundler patchelfs its own RPATH and
  # installs contrib via `info.extraFiles`).
  postInstall =
    if portable then ''
      # No-op — bundler handles staging.
      true
    '' else ''
      install -Dm0644 bins/ansyncd/contrib/60-ansync-uinput.rules \
        "$out/lib/udev/rules.d/60-ansync-uinput.rules"
      install -Dm0644 bins/ansyncd/contrib/61-ansync-v4l2loopback.rules \
        "$out/lib/udev/rules.d/61-ansync-v4l2loopback.rules"
      install -Dm0644 bins/ansyncd/contrib/ansync-v4l2loopback.conf \
        "$out/lib/modprobe.d/ansync-v4l2loopback.conf"
      install -Dm0644 bins/ansyncd/contrib/ansync-modules-load.conf \
        "$out/lib/modules-load.d/ansync.conf"
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
