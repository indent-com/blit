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

        version = "0.1.2";

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # Static musl toolchain + platform for .deb builds
        muslTarget = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64-unknown-linux-musl" else "x86_64-unknown-linux-musl";

        rustToolchainMusl = pkgs.rust-bin.stable.latest.default.override {
          targets = [ muslTarget ];
        };

        rustPlatformMusl = pkgs.makeRustPlatform {
          cargo = rustToolchainMusl;
          rustc = rustToolchainMusl;
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

        blit-client-bundler = rustPlatform.buildRustPackage {
          pname = "blit-client-bundler";
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

        blit-client-node = rustPlatform.buildRustPackage {
          pname = "blit-client-node";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit" ];
          cargoLock.lockFile = ./Cargo.lock;
          nativeBuildInputs = [ pkgs.wasm-pack pkgs.wasm-bindgen-cli pkgs.binaryen ];
          buildPhase = ''
            cd npm
            HOME=$TMPDIR wasm-pack build --target nodejs --release --out-dir $out
          '';
          dontInstall = true;
          doCheck = false;
        };

        npm-publish = pkgs.writeShellApplication {
          name = "npm-publish";
          runtimeInputs = [ pkgs.nodejs ];
          text = ''
            tmp=$(mktemp -d)
            trap 'rm -rf "$tmp"' EXIT

            # Start with the bundler build (has blit.js as ESM entry + blit_bg.js + .wasm + .d.ts)
            cp -a ${blit-client-bundler}/* "$tmp"/
            chmod -R u+w "$tmp"

            # Add the node entry from the nodejs build
            cp ${blit-client-node}/blit.js "$tmp/blit_node.js"

            cat > "$tmp/package.json" <<'PKGJSON'
            {
              "name": "blit-client",
              "version": "${version}",
              "description": "Low-latency terminal streaming — JS+WASM client",
              "main": "blit_node.js",
              "module": "blit.js",
              "types": "blit.d.ts",
              "files": ["blit_bg.wasm","blit_bg.js","blit_bg.wasm.d.ts","blit.js","blit.d.ts","blit_node.js"],
              "keywords": ["terminal","tty","wasm","streaming"],
              "license": "MIT",
              "repository": {"type":"git","url":"git+https://github.com/indent-com/blit.git"}
            }
            PKGJSON
            sed -i 's/^            //' "$tmp/package.json"
            echo "Package contents:"
            ls -lh "$tmp"
            echo ""
            npm publish "$tmp" "$@"
          '';
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
          meta.mainProgram = "blit";
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

        # Static musl builds for .deb packages
        mkMuslBin = { pname, cargoPkg, extraArgs ? {} }: rustPlatformMusl.buildRustPackage ({
          inherit pname version;
          src = ./.;
          cargoBuildFlags = [ "-p" cargoPkg ];
          cargoLock.lockFile = ./Cargo.lock;
          doCheck = false;
          CARGO_BUILD_TARGET = muslTarget;
          CARGO_BUILD_RUSTFLAGS = "-C target-feature=+crt-static";
        } // extraArgs);

        blit-server-static = mkMuslBin {
          pname = "blit-server";
          cargoPkg = "blit-server";
        };

        blit-cli-static = mkMuslBin {
          pname = "blit-cli";
          cargoPkg = "blit-cli";
        };

        blit-gateway-static = mkMuslBin {
          pname = "blit-gateway";
          cargoPkg = "blit-gateway";
          extraArgs = {
            preBuild = ''
              mkdir -p web
              cp ${browserWasm}/blit_browser_bg.wasm web/
              cp ${browserWasm}/blit_browser.js web/
            '';
          };
        };

        mkDeb = { pname, binName ? pname, binPkg, description }: pkgs.stdenv.mkDerivation {
          pname = "${pname}-deb";
          inherit version;
          nativeBuildInputs = [ pkgs.dpkg ];
          dontUnpack = true;
          buildPhase =
            let arch = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
            in ''
              mkdir -p pkg/DEBIAN pkg/usr/bin
              cp ${binPkg}/bin/${binName} pkg/usr/bin/
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
        packages.blit-gateway = blit-gateway;
        packages.blit-client = blit-client-bundler;
        packages.npm-publish = npm-publish;
        packages.default = blit-cli;

        packages.blit-server-deb = mkDeb {
          pname = "blit-server";
          binPkg = blit-server-static;
          description = "blit terminal streaming server";
        };
        packages.blit-cli-deb = mkDeb {
          pname = "blit";
          binPkg = blit-cli-static;
          description = "blit terminal client";
        };
        packages.blit-gateway-deb = mkDeb {
          pname = "blit-gateway";
          binPkg = blit-gateway-static;
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
