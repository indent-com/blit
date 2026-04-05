{ inputs, system }:
let
  pkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ inputs.rust-overlay.overlays.default ];
  };

  version = "0.20.0";

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
        || pkgs.lib.hasInfix "/man/" path
        || pkgs.lib.hasInfix "/js/ui/dist/" path;
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
      pkgs.libxkbcommon
      pkgs.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.ffmpeg-headless
      pkgs.libva
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
  craneLibStatic = (inputs.crane.mkLib pkgs.pkgsStatic).overrideToolchain rustToolchain;

  commonArgsStatic = {
    inherit src version;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      pkgs.pkgsStatic.libxkbcommon
      pkgs.pkgsStatic.pixman
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
