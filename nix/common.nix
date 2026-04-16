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
    ];
    # Link musl libc dynamically so dlopen works (GPU acceleration).
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

  # ------------------------------------------------------------------
  # Glibc + cargo-zigbuild for portable Linux release binaries.
  #
  # cargo-zigbuild uses zig as the linker to enforce a glibc version
  # floor.  It also sets CC/CXX to zig wrappers for C deps.
  #
  # aws-lc-sys's default cc-crate builder falsely detects zig cc as
  # a buggy GCC (memcmp check).  Setting AWS_LC_SYS_CMAKE_BUILDER=1
  # switches it to cmake, which identifies zig cc as Clang and works.
  # ------------------------------------------------------------------

  minGlibcVersion = "2.31";

  rustTargetGnu =
    if pkgs.stdenv.hostPlatform.isAarch64
    then "aarch64-unknown-linux-gnu"
    else "x86_64-unknown-linux-gnu";

  # Static libopus for the glibc release build so the binary is
  # fully self-contained (only glibc itself is dynamic).
  gnuStaticLibopus = pkgs.libopus.overrideAttrs (old: {
    mesonFlags = (old.mesonFlags or [ ]) ++ [ "-Ddefault_library=static" ];
  });

  commonArgsGnu = {
    inherit src version;
    strictDeps = true;
    nativeBuildInputs = [
      pkgs.pkg-config
      pkgs.llvmPackages.libclang
      pkgs.cargo-zigbuild
      pkgs.zig
      pkgs.cmake # needed by aws-lc-sys cmake builder
    ];
    buildInputs = [
      gnuStaticLibopus
    ];
    BINDGEN_EXTRA_CLANG_ARGS = "-isystem ${pkgs.lib.getDev pkgs.stdenv.cc.libc}/include";
    LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
    CARGO_BUILD_TARGET = rustTargetGnu;
    # Use cmake builder for aws-lc-sys to avoid its false-positive
    # zig-cc memcmp bug check in the cc-crate code path.
    AWS_LC_SYS_CMAKE_BUILDER = "1";
    # Force static linking of opus — zig's linker rejects .so files
    # built against a newer glibc than the target version.
    OPUS_STATIC = "1";
  };

  cargoArtifactsGnu = craneLib.buildDepsOnly (
    commonArgsGnu
    // {
      pname = "blit-workspace-deps-gnu";
      cargoExtraArgs = "--workspace --exclude blit-browser";
      doCheck = false;
      buildPhaseCargoCommand = "HOME=$TMPDIR cargo zigbuild --profile release --target ${rustTargetGnu}.${minGlibcVersion} --workspace --exclude blit-browser";
      doNotPostBuildInstallCargoBinaries = true;
    }
  );

in
{
  inherit
    pkgs
    pkgsStaticLLVM
    version
    minGlibcVersion
    rustTargetGnu
    cargoLockConfig
    rustToolchain
    rustPlatform
    craneLib
    craneLibStatic
    src
    commonArgs
    commonArgsGnu
    commonArgsStatic
    cargoArtifacts
    cargoArtifactsGnu
    cargoArtifactsStatic
    ;
}
