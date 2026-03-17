{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        browserWasm = rustPlatform.buildRustPackage {
          pname = "blit-browser";
          version = "0.1.0";
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-browser" ];
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.wasm-pack pkgs.wasm-bindgen-cli pkgs.binaryen ];
          buildPhase = ''
            cd browser
            HOME=$TMPDIR wasm-pack build --target web --release --out-dir $out
          '';
          dontInstall = true;
          doCheck = false;
        };
      in
      {
        packages.blit-server = rustPlatform.buildRustPackage {
          pname = "blit-server";
          version = "0.1.0";
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-server" ];
          cargoLock.lockFile = ./Cargo.lock;
        };

        packages.default = rustPlatform.buildRustPackage {
          pname = "blit-gateway";
          version = "0.1.0";
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-gateway" ];
          cargoLock.lockFile = ./Cargo.lock;
          preBuild = ''
            mkdir -p web
            cp ${browserWasm}/blit_browser_bg.wasm web/
            cp ${browserWasm}/blit_browser.js web/
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.wasm-pack
            pkgs.wasm-bindgen-cli
            pkgs.binaryen
          ];

          shellHook = ''
            echo "blit dev shell"
            echo "  build browser wasm: cd browser && wasm-pack build --target web --release --out-dir ../web"
            echo "  run server:         cargo run -p blit-server"
            echo "  run gateway:        BLIT_PASS=secret cargo run -p blit-gateway  # http://localhost:3264"
          '';
        };
      }
    );
}
