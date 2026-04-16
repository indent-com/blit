{ inputs, system }:
let
  pkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ inputs.rust-overlay.overlays.default ];
  };

  version = "0.24.1";

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

  # LLVM-based musl static environment for release tarballs.
  # Combines isStatic (musl) + useLLVM (clang/lld/compiler-rt/libc++)
  # so we get PIC-clean C++ libs and avoid the libgcc_s hacks that the
  # GCC musl toolchain requires.
  pkgsStaticLLVM =
    if pkgs.stdenv.isLinux then
      import inputs.nixpkgs {
        inherit system;
        overlays = [ inputs.rust-overlay.overlays.default ];
        crossSystem = {
          isStatic = true;
          useLLVM = true;
          linker = "lld";
          config = pkgs.lib.systems.parse.tripleFromSystem (
            pkgs.lib.systems.parse.mkMuslSystem pkgs.stdenv.hostPlatform.parsed
          );
        };
      }
    else
      # macOS doesn't cross-compile to musl — use pkgsStatic as-is.
      pkgs.pkgsStatic;

  craneLibStatic = (inputs.crane.mkLib pkgsStaticLLVM).overrideToolchain (
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
  staticLibgbm = pkgsStaticLLVM.libgbm.overrideAttrs (old: {
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
      pkgsStaticLLVM.libopus.overrideAttrs (old: {
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
      pkgsStaticLLVM.libopus;

  commonArgsStatic = {
    inherit src version;
    strictDeps = true;
    nativeBuildInputs = [ pkgs.pkg-config ];
    buildInputs = [
      staticLibopus
      pkgsStaticLLVM.libxkbcommon
      pkgsStaticLLVM.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      staticLibgbm
    ];
    # Link musl libc dynamically so dlopen works (GPU acceleration).
    # All other deps (libopus, pixman, etc.) remain statically linked.
    RUSTFLAGS = "-C target-feature=-crt-static";
  }
  // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
    CARGO_BUILD_TARGET = pkgsStaticLLVM.stdenv.hostPlatform.rust.rustcTargetSpec;
    # Tell the `cc` crate to link libc++ instead of libstdc++ (LLVM toolchain).
    CXXSTDLIB = "c++";
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.lib.getDev pkgsStaticLLVM.stdenv.cc.libc}/include";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.llvmPackages.libclang
    ];
    # Rustc hardcodes `-lgcc_s` for dynamic-musl targets.  With the
    # LLVM toolchain we provide compiler-rt builtins under that name.
    # Scoped to the musl target via NIX_LDFLAGS_<role> so the glibc CC
    # used for build scripts is unaffected.
    postUnpack =
      let
        compilerRt = pkgsStaticLLVM.llvmPackages.compiler-rt;
        role = builtins.replaceStrings [ "-" ] [ "_" ]
          pkgsStaticLLVM.stdenv.hostPlatform.rust.rustcTargetSpec;
      in
      ''
        export NIX_CFLAGS_LINK=""
        mkdir -p $TMPDIR/rt-compat
        builtins_lib=$(echo ${compilerRt}/lib/*/libclang_rt.builtins-*.a)
        ln -s "$builtins_lib" $TMPDIR/rt-compat/libgcc_s.a
        export NIX_LDFLAGS_${role}="-L$TMPDIR/rt-compat ''${NIX_LDFLAGS_${role}:-}"
      '';
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
    pkgsStaticLLVM
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
