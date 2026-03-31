{ inputs, system }:
let
  pkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ inputs.rust-overlay.overlays.default ];
  };

  version = "0.13.1";

  cargoLockConfig = {
    lockFile = ../Cargo.lock;
  };

  rustToolchain = pkgs.rust-bin.stable.latest.default.override {
    targets = [ "wasm32-unknown-unknown" "x86_64-unknown-linux-musl" "aarch64-unknown-linux-musl" ];
    extensions = [ "clippy" "llvm-tools" ];
  };

  rustPlatform = pkgs.makeRustPlatform {
    cargo = rustToolchain;
    rustc = rustToolchain;
  };
in {
  inherit pkgs version cargoLockConfig rustToolchain rustPlatform;
}
