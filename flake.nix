{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    {
      nixosModules.default = self.nixosModules.blit;
      nixosModules.blit = { config, lib, pkgs, ... }:
        let
          cfg = config.services.blit;
          inherit (lib) mkEnableOption mkOption types mkIf;
        in {
          options.services.blit = {
            enable = mkEnableOption "blit terminal multiplexer";

            package = mkOption {
              type = types.package;
              default = self.packages.${pkgs.system}.blit-server;
              defaultText = "self.packages.\${system}.blit-server";
              description = "The blit-server package to use.";
            };

            users = mkOption {
              type = types.listOf types.str;
              default = [];
              example = [ "alice" "bob" ];
              description = ''
                Users to enable blit for. Each user gets a socket-activated
                blit-server instance at /run/blit/<user>.sock.
              '';
            };

            shell = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "/run/current-system/sw/bin/bash";
              description = "Shell to spawn for new PTYs. Defaults to the user's login shell.";
            };

            scrollback = mkOption {
              type = types.int;
              default = 10000;
              description = "Scrollback buffer size in rows per PTY.";
            };

            gateways = mkOption {
              type = types.attrsOf (types.submodule {
                options = {
                  user = mkOption {
                    type = types.str;
                    description = "User whose blit-server socket to connect to.";
                  };
                  port = mkOption {
                    type = types.port;
                    default = 3264;
                    description = "Port to listen on.";
                  };
                  addr = mkOption {
                    type = types.str;
                    default = "0.0.0.0";
                    description = "Address to bind to.";
                  };
                  passFile = mkOption {
                    type = types.path;
                    description = "File containing the gateway passphrase.";
                  };
                  package = mkOption {
                    type = types.package;
                    default = self.packages.${pkgs.system}.blit-gateway;
                    defaultText = "self.packages.\${system}.blit-gateway";
                    description = "The blit-gateway package to use.";
                  };
                };
              });
              default = {};
              description = "Named blit-gateway instances connecting to blit-server sockets.";
            };
          };

          config = mkIf cfg.enable {
            systemd.services = builtins.listToAttrs (map (user: {
              name = "blit@${user}";
              value = {
                description = "blit terminal multiplexer for ${user}";
                requires = [ "blit@${user}.socket" ];
                serviceConfig = {
                  Type = "simple";
                  User = user;
                  WorkingDirectory = "~";
                  ExecStart = let
                    serverBin = "${cfg.package}/bin/blit-server";
                  in "${serverBin}";
                  Environment = lib.optional (cfg.shell != null) "SHELL=${cfg.shell}"
                    ++ [ "BLIT_SCROLLBACK=${toString cfg.scrollback}" ];
                };
              };
            }) cfg.users)
            // builtins.listToAttrs (lib.mapAttrsToList (name: gw: {
              name = "blit-gateway-${name}";
              value = {
                description = "blit gateway ${name} for ${gw.user}";
                after = [ "blit@${gw.user}.socket" "network.target" ];
                requires = [ "blit@${gw.user}.socket" ];
                wantedBy = [ "multi-user.target" ];
                serviceConfig = {
                  Type = "simple";
                  ExecStart = "${gw.package}/bin/blit-gateway";
                  Environment = [
                    "BLIT_SOCK=/run/blit/${gw.user}.sock"
                    "BLIT_ADDR=${gw.addr}:${toString gw.port}"
                  ];
                  EnvironmentFile = gw.passFile;
                };
              };
            }) cfg.gateways);

            systemd.sockets = builtins.listToAttrs (map (user: {
              name = "blit@${user}";
              value = {
                description = "blit terminal multiplexer socket for ${user}";
                wantedBy = [ "sockets.target" ];
                socketConfig = {
                  ListenStream = "/run/blit/${user}.sock";
                  SocketUser = user;
                  SocketMode = "0700";
                  RuntimeDirectory = "blit";
                  RuntimeDirectoryMode = "0755";
                };
              };
            }) cfg.users);
          };
        };
    } //
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        version = "0.2.1";

        weztermHash = "sha256-V6WvkNZryYofarsyfcmsuvtpNJ/c3O+DmOKNvoYPbmA=";
        finlUnicodeHash = "sha256-38S6XH4hldbkb6NP+s7lXa/NR49PI0w3KYqd+jPHND0=";
        cargoLockConfig = {
          lockFile = ./Cargo.lock;
          outputHashes =
            { "finl_unicode-1.3.0" = finlUnicodeHash; }
            // builtins.listToAttrs (map (name: { inherit name; value = weztermHash; }) [
              "filedescriptor-0.8.3"
              "termwiz-0.24.0"
              "vtparse-0.7.0"
              "wezterm-bidi-0.2.3"
              "wezterm-blob-leases-0.1.1"
              "wezterm-cell-0.1.0"
              "wezterm-char-props-0.1.3"
              "wezterm-color-types-0.3.0"
              "wezterm-dynamic-0.2.1"
              "wezterm-dynamic-derive-0.1.1"
              "wezterm-escape-parser-0.1.0"
              "wezterm-input-types-0.1.0"
              "wezterm-surface-0.1.0"
              "wezterm-term-0.1.0"
            ]);
        };

        # wezterm-term uses include_bytes!("../../../termwiz/data/wezterm") which
        # reaches outside its crate into the wezterm monorepo.  When nix vendors
        # crates individually, that path doesn't exist.  Place it where the
        # include_bytes! expects it relative to the cargo vendor dir.
        # wezterm-term's include_bytes! references a terminfo file via a
        # relative path that escapes the crate root into the wezterm monorepo.
        # When nix vendors the crate, that path doesn't exist.  Fix it by
        # placing the file where the include_bytes! expects it.
        patchWeztermTerminfo = ''
          for d in "cargo-vendor-dir" "$NIX_BUILD_TOP/cargo-vendor-dir"; do
            if [ -d "$d/wezterm-term-0.1.0" ]; then
              mkdir -p "$d/termwiz/data"
              cp ${./vendor-patches/wezterm-terminfo} "$d/termwiz/data/wezterm"
              echo "patched wezterm-term vendor at $d"
              break
            fi
          done
        '';

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" "x86_64-unknown-linux-musl" ];
          extensions = [ "llvm-tools" ];
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
          cargoLock = cargoLockConfig;
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
          cargoLock = cargoLockConfig;
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
          cargoLock = cargoLockConfig;
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
            echo "Package contents:"
            ls -lh "$tmp"
            echo ""
            npm publish "$tmp" "$@"
          '';
        };

        react-publish = pkgs.writeShellApplication {
          name = "react-publish";
          runtimeInputs = [ pkgs.nodejs ];
          text = ''
            tmp=$(mktemp -d)
            trap 'rm -rf "$tmp"' EXIT

            # Set up blit-browser as a resolvable package for tsc
            wasm="$tmp/blit-browser"
            mkdir -p "$wasm"
            cp ${browserWasm}/blit_browser.js ${browserWasm}/blit_browser.d.ts "$wasm"/
            # Also grab the .wasm.d.ts if present
            cp ${browserWasm}/blit_browser_bg.wasm.d.ts "$wasm"/ 2>/dev/null || true
            echo '{"name":"blit-browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > "$wasm/package.json"

            # Copy the react package source
            cp -a ${./react}/* "$tmp"/
            chmod -R u+w "$tmp"

            cd "$tmp"
            # Point the devDependency at our local WASM build
            ${pkgs.nodejs}/bin/npm pkg set "devDependencies.blit-browser=file:$wasm"
            npm install
            npm run build

            echo "Package contents:"
            ls -lh dist/
            echo ""
            npm publish "$@"
          '';
        };

        blit-server = rustPlatform.buildRustPackage {
          pname = "blit-server";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-server" ];
          cargoLock = cargoLockConfig;
          preBuild = patchWeztermTerminfo;
          doCheck = false;
        };

        blit-cli = rustPlatform.buildRustPackage {
          pname = "blit-cli";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-cli" ];
          cargoLock = cargoLockConfig;
          preBuild = copyWebAssets;
          doCheck = false;
          meta.mainProgram = "blit";
        };

        blit-gateway = rustPlatform.buildRustPackage {
          pname = "blit-gateway";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-gateway" ];
          cargoLock = cargoLockConfig;
          preBuild = ''
            mkdir -p web
            cp ${browserWasm}/blit_browser_bg.wasm web/
            cp ${browserWasm}/blit_browser.js web/
          '';
          doCheck = false;
        };

        # Static musl builds for .deb packages (Linux only).
        # pkgsStatic provides a full musl stdenv; using rust-bin toolchain from
        # the overlay ensures consistency with the rest of the flake.
        rustToolchainStatic = pkgs.pkgsStatic.rust-bin.stable.latest.default;

        rustPlatformStatic = pkgs.pkgsStatic.makeRustPlatform {
          cargo = rustToolchainStatic;
          rustc = rustToolchainStatic;
        };

        mkStaticBin = { pname, cargoPkg, extraArgs ? {} }: rustPlatformStatic.buildRustPackage ({
          inherit pname version;
          src = ./.;
          cargoBuildFlags = [ "-p" cargoPkg ];
          cargoLock = cargoLockConfig;
          preBuild = patchWeztermTerminfo;
          doCheck = false;
        } // extraArgs);

        blit-server-static = mkStaticBin {
          pname = "blit-server";
          cargoPkg = "blit-server";
        };

        copyWebAssets = ''
          mkdir -p web/snippets
          cp ${browserWasm}/blit_browser.js web/
          cp ${browserWasm}/blit_browser_bg.wasm web/
          cp ${browserWasm}/blit_browser.d.ts web/
          cp ${browserWasm}/blit_browser_bg.wasm.d.ts web/
          for d in ${browserWasm}/snippets/blit-browser-*/; do
            name=$(basename "$d")
            mkdir -p "web/snippets/$name"
            cp "$d"/* "web/snippets/$name/"
          done
        '';

        blit-cli-static = mkStaticBin {
          pname = "blit-cli";
          cargoPkg = "blit-cli";
          extraArgs = { preBuild = copyWebAssets; };
        };

        blit-gateway-static = mkStaticBin {
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


        mkDeb = { pname, binName ? pname, binPkg, description, extraInstall ? "" }: pkgs.stdenv.mkDerivation {
          pname = "${pname}-deb";
          inherit version;
          nativeBuildInputs = [ pkgs.dpkg ];
          dontUnpack = true;
          buildPhase =
            let arch = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
            in ''
              mkdir -p pkg/DEBIAN pkg/usr/bin
              cp ${binPkg}/bin/${binName} pkg/usr/bin/
              ${extraInstall}
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
        packages.react-publish = react-publish;
        packages.default = blit-cli;

        packages.blit-server-deb = mkDeb {
          pname = "blit-server";
          binPkg = blit-server-static;
          description = "blit terminal streaming server";
          extraInstall = let
            socketUnit = ./systemd + "/blit@.socket";
            serviceUnit = ./systemd + "/blit@.service";
          in ''
            mkdir -p pkg/lib/systemd/system
            cp ${socketUnit} pkg/lib/systemd/system/blit@.socket
            cp ${serviceUnit} pkg/lib/systemd/system/blit@.service
          '';
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

        packages.tests = pkgs.writeShellApplication {
          name = "blit-tests";
          runtimeInputs = [ rustToolchain pkgs.nodejs pkgs.pnpm ];
          text = ''
            echo "=== Rust tests ==="
            cargo test --workspace
            echo ""
            echo "=== React tests ==="
            (cd react && pnpm install --frozen-lockfile 2>/dev/null || pnpm install && pnpm vitest run)
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.wasm-pack
            pkgs.wasm-bindgen-cli
            pkgs.binaryen
            pkgs.pkgsStatic.stdenv.cc
            pkgs.process-compose
            pkgs.cargo-watch
            pkgs.cargo-llvm-cov
          ];

          shellHook = ''
            echo "blit dev shell"
            echo "  dev:                dev  (server + gateway + browser assets, auto-reload on source change)"
            echo "  build:              build"
            echo "  run server:         cargo run -p blit-server"
            echo "  run gateway:        BLIT_PASS=secret cargo run -p blit-gateway  # http://localhost:3264"
            echo "  run cli:            cargo run -p blit-cli"
          '';
        };
      }
    );
}
