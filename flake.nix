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

        version = "0.1.0";

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        browserWasm = rustPlatform.buildRustPackage {
          pname = "blit-browser";
          inherit version;
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

        npmPkg = rustPlatform.buildRustPackage {
          pname = "blit-npm";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit" ];
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.wasm-pack pkgs.wasm-bindgen-cli pkgs.binaryen ];
          buildPhase = ''
            cd npm
            HOME=$TMPDIR wasm-pack build --target bundler --release --out-dir $out
          '';
          dontInstall = true;
          doCheck = false;
        };

        blit-server = rustPlatform.buildRustPackage {
          pname = "blit-server";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-server" ];
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false;
        };

        blit-cli = rustPlatform.buildRustPackage {
          pname = "blit-cli";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-cli" ];
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false;
        };

        blit-gateway = rustPlatform.buildRustPackage {
          pname = "blit-gateway";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-gateway" ];
          cargoLock.lockFile = ./Cargo.lock;
          preBuild = ''
            mkdir -p web
            cp ${browserWasm}/blit_browser_bg.wasm web/
            cp ${browserWasm}/blit_browser.js web/
          '';
          doCheck = false;
        };

        mkDeb = { pname, binPkg, description }: pkgs.stdenv.mkDerivation {
          pname = "${pname}-deb";
          inherit version;
          nativeBuildInputs = [ pkgs.dpkg ];
          dontUnpack = true;
          buildPhase =
            let arch = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
            in ''
              mkdir -p pkg/DEBIAN pkg/usr/bin
              cp ${binPkg}/bin/${pname} pkg/usr/bin/
              cat > pkg/DEBIAN/control <<'CTRL'
Package: ${pname}
Version: ${version}
Architecture: ${arch}
Maintainer: Pierre Carrier
Description: ${description}
CTRL
              mkdir -p $out
              dpkg-deb --build pkg $out/${pname}_${version}_${arch}.deb
            '';
          installPhase = "true";
        };
      in
      {
        packages.blit-server = blit-server;
        packages.blit-cli = blit-cli;
        packages.blit-npm = npmPkg;
        packages.default = blit-gateway;

        packages.blit-server-deb = mkDeb {
          pname = "blit-server";
          binPkg = blit-server;
          description = "blit terminal streaming server";
        };
        packages.blit-cli-deb = mkDeb {
          pname = "blit-cli";
          binPkg = blit-cli;
          description = "blit terminal client";
        };
        packages.blit-gateway-deb = mkDeb {
          pname = "blit-gateway";
          binPkg = blit-gateway;
          description = "blit WebSocket gateway";
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
            echo "  run cli:            cargo run -p blit-cli"
          '';
        };
      }
    );
}
