{ inputs, system }:
let
  pkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ inputs.rust-overlay.overlays.default ];
  };

  version = "0.23.0";

  cargoLockConfig = {
    lockFile = ../Cargo.lock;
    outputHashes = {
      "alacritty_terminal-0.25.1" = "sha256-YjUnHTEIjeLyQY8gXCWf+3WQU5WYlbcYIKM0ZACqnTc=";
    };
  };

  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [
      "wasm32-unknown-unknown"
      "x86_64-unknown-linux-musl"
      "aarch64-unknown-linux-musl"
    ];
    extensions = [
      "clippy"
      "llvm-tools"
    ];
  };

  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };

  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

  # Shared source filtering — only include Rust/Cargo files + assets crane needs.
  src =
    let
      # Keep Cargo manifests, Rust source, build scripts, and non-Rust assets
      # the build needs (web dist, man pages, etc.).
      filter =
        path: type:
        (craneLib.filterCargoSources path type)
        || pkgs.lib.hasSuffix ".html" path
        || pkgs.lib.hasSuffix ".html.br" path
        || builtins.baseNameOf path == "learn.md"
        || pkgs.lib.hasInfix "/js/ui/dist/" path
        || pkgs.lib.hasSuffix ".xkb" path
        || pkgs.lib.hasSuffix ".spv" path;
    in
    pkgs.lib.cleanSourceWith {
      src = ../.;
      inherit filter;
    };

  # Common args shared by all crane builds.
  commonArgs = {
    inherit src version;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      pkgs.libopus # system Opus for audiopus_sys (avoids cmake source build)
      pkgs.libxkbcommon
      pkgs.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.ffmpeg-headless
      pkgs.libglvnd # EGL / GLESv2 dispatch
      pkgs.libva
      pkgs.libgbm # libgbm for GBM device / buffer allocation
      pkgs.vulkan-loader # libvulkan.so.1 for Vulkan compositor renderer
    ];
    nativeCheckInputs = [ ];
  }
  // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.lib.getDev pkgs.stdenv.cc.libc}/include";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.llvmPackages.libclang
    ];
  };

  # Build workspace deps once — reused by the workspace build.
  cargoArtifacts = craneLib.buildDepsOnly (
    commonArgs
    // {
      pname = "blit-workspace-deps";
      cargoExtraArgs = "--workspace --exclude blit-browser";
      doCheck = false;
    }
  );

  # Static (musl on Linux) Crane setup for release tarballs.
  craneLibStatic = (inputs.crane.mkLib pkgs.pkgsStatic).overrideToolchain (
    p:
    p.rust-bin.stable.latest.default.override {
      targets = [
        "wasm32-unknown-unknown"
        "x86_64-unknown-linux-musl"
        "aarch64-unknown-linux-musl"
      ];
      extensions = [
        "clippy"
        "llvm-tools"
      ];
    }
  );

  # Mesa's meson.build uses shared_library() for libgbm, which the musl
  # static toolchain cannot link.  Override to use library() so meson
  # respects --default-library=static and produces libgbm.a.
  staticLibgbm = pkgs.pkgsStatic.libgbm.overrideAttrs (old: {
    postPatch = (old.postPatch or "") + ''
      substituteInPlace src/gbm/meson.build \
        --replace-fail "shared_library(" "library("
    '';
  });

  # Opus's meson.build doesn't support arm64 intrinsics, so the default
  # -Dintrinsics=enabled fails on aarch64 in pkgsStatic.  Disable
  # intrinsics and rtcd (runtime CPU detection depends on intrinsics).
  staticLibopus =
    if pkgs.stdenv.hostPlatform.isAarch64 then
      pkgs.pkgsStatic.libopus.overrideAttrs (old: {
        mesonFlags = builtins.map (
          f:
          if f == "-Dintrinsics=enabled" then
            "-Dintrinsics=disabled"
          else if f == "-Drtcd=enabled" then
            "-Drtcd=disabled"
          else
            f
        ) (old.mesonFlags or [ ]);
      })
    else
      pkgs.pkgsStatic.libopus;

  commonArgsStatic = {
    inherit src version;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      staticLibopus
      pkgs.pkgsStatic.libxkbcommon
      pkgs.pkgsStatic.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      staticLibgbm
    ];
    RUSTFLAGS = "-C relocation-model=static";
  }
  // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
    CARGO_BUILD_TARGET = pkgs.pkgsStatic.stdenv.hostPlatform.rust.rustcTargetSpec;
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.lib.getDev pkgs.pkgsStatic.stdenv.cc.libc}/include";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.llvmPackages.libclang
    ];
    postUnpack = "export NIX_CFLAGS_LINK=''";
  };

  cargoArtifactsStatic = craneLibStatic.buildDepsOnly (
    commonArgsStatic
    // {
      pname = "blit-workspace-deps-static";
      cargoExtraArgs = "--workspace --exclude blit-browser";
      doCheck = false;
    }
  );

in
{
  inherit
    pkgs
    version
    cargoLockConfig
    rustToolchain
    rustPlatform
    craneLib
    craneLibStatic
    src
    commonArgs
    commonArgsStatic
    cargoArtifacts
    cargoArtifactsStatic
    ;
}
