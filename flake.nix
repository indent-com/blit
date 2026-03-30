{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    {
      darwinModules.default = self.darwinModules.blit;
      darwinModules.blit = { config, lib, pkgs, ... }:
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

            shell = mkOption {
              type = types.nullOr types.str;
              default = null;
              example = "/run/current-system/sw/bin/fish";
              description = "Shell to spawn for new PTYs. Defaults to the user's login shell.";
            };

            scrollback = mkOption {
              type = types.int;
              default = 10000;
              description = "Scrollback buffer size in rows per PTY.";
            };

            socketPath = mkOption {
              type = types.str;
              default = "/tmp/blit.sock";
              description = "Unix socket path for blit-server.";
            };

            gateways = mkOption {
              type = types.attrsOf (types.submodule {
                options = {
                  port = mkOption {
                    type = types.port;
                    default = 3264;
                    description = "Port to listen on.";
                  };
                  addr = mkOption {
                    type = types.str;
                    default = "127.0.0.1";
                    description = "Address to bind to.";
                  };
                  passFile = mkOption {
                    type = types.path;
                    description = "File containing BLIT_PASS=<passphrase>.";
                  };
                  fontDirs = mkOption {
                    type = types.listOf types.str;
                    default = [];
                    example = [ "/Library/Fonts" "~/Library/Fonts" ];
                    description = "Extra font directories to search.";
                  };
                  quic = mkOption {
                    type = types.bool;
                    default = false;
                    description = "Enable WebTransport (QUIC/HTTP3) alongside WebSocket.";
                  };
                  tlsCert = mkOption {
                    type = types.nullOr types.path;
                    default = null;
                    description = "PEM certificate file for WebTransport TLS. Auto-generated if null.";
                  };
                  tlsKey = mkOption {
                    type = types.nullOr types.path;
                    default = null;
                    description = "PEM private key file for WebTransport TLS. Auto-generated if null.";
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
              description = "Named blit-gateway instances.";
            };
          };

          config = mkIf cfg.enable {
            launchd.user.agents = {
              blit = {
                serviceConfig = {
                  Label = "com.blit.server";
                  ProgramArguments = [
                    "/bin/sh" "-lc"
                    ''[ -n "$LANG" ] || export LANG="$(defaults read -g AppleLocale 2>/dev/null | sed 's/@.*//' || echo en_US).UTF-8"; exec ${cfg.package}/bin/blit-server''
                  ];
                  EnvironmentVariables = {
                    BLIT_SOCK = cfg.socketPath;
                    BLIT_SCROLLBACK = toString cfg.scrollback;
                  } // lib.optionalAttrs (cfg.shell != null) {
                    SHELL = cfg.shell;
                  };
                  RunAtLoad = true;
                  KeepAlive = true;
                  StandardOutPath = "/tmp/blit-server.log";
                  StandardErrorPath = "/tmp/blit-server.log";
                };
              };
            } // builtins.listToAttrs (lib.mapAttrsToList (name: gw: {
              name = "blit-gateway-${name}";
              value = {
                serviceConfig = {
                  Label = "com.blit.gateway.${name}";
                  ProgramArguments = [
                    "/bin/sh" "-c"
                    ''. ${gw.passFile} && exec ${gw.package}/bin/blit-gateway''
                  ];
                  EnvironmentVariables = {
                    BLIT_SOCK = cfg.socketPath;
                    BLIT_ADDR = "${gw.addr}:${toString gw.port}";
                  } // lib.optionalAttrs (gw.fontDirs != []) {
                    BLIT_FONT_DIRS = lib.concatStringsSep ":" gw.fontDirs;
                  } // lib.optionalAttrs gw.quic {
                    BLIT_QUIC = "1";
                  } // lib.optionalAttrs (gw.tlsCert != null) {
                    BLIT_TLS_CERT = gw.tlsCert;
                  } // lib.optionalAttrs (gw.tlsKey != null) {
                    BLIT_TLS_KEY = gw.tlsKey;
                  };
                  RunAtLoad = true;
                  KeepAlive = true;
                  StandardOutPath = "/tmp/blit-gateway-${name}.log";
                  StandardErrorPath = "/tmp/blit-gateway-${name}.log";
                };
              };
            }) cfg.gateways);
          };
        };

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
                  fontDirs = mkOption {
                    type = types.listOf types.str;
                    default = [];
                    example = [ "/usr/share/fonts" "/home/alice/.local/share/fonts" ];
                    description = "Extra font directories to search.";
                  };
                  quic = mkOption {
                    type = types.bool;
                    default = false;
                    description = "Enable WebTransport (QUIC/HTTP3) alongside WebSocket.";
                  };
                  tlsCert = mkOption {
                    type = types.nullOr types.path;
                    default = null;
                    description = "PEM certificate file for WebTransport TLS. Auto-generated if null.";
                  };
                  tlsKey = mkOption {
                    type = types.nullOr types.path;
                    default = null;
                    description = "PEM private key file for WebTransport TLS. Auto-generated if null.";
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
                  User = gw.user;
                  ExecStart = "${gw.package}/bin/blit-gateway";
                  Environment = [
                    "BLIT_SOCK=/run/blit/${gw.user}.sock"
                    "BLIT_ADDR=${gw.addr}:${toString gw.port}"
                  ] ++ lib.optional (gw.fontDirs != []) "BLIT_FONT_DIRS=${lib.concatStringsSep ":" gw.fontDirs}"
                    ++ lib.optional gw.quic "BLIT_QUIC=1"
                    ++ lib.optional (gw.tlsCert != null) "BLIT_TLS_CERT=${gw.tlsCert}"
                    ++ lib.optional (gw.tlsKey != null) "BLIT_TLS_KEY=${gw.tlsKey}";
                  EnvironmentFile = gw.passFile;
                  AmbientCapabilities = lib.mkIf (gw.port < 1024) [ "CAP_NET_BIND_SERVICE" ];
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

        version = "0.11.0";

        cargoLockConfig = {
          lockFile = ./Cargo.lock;
        };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" "x86_64-unknown-linux-musl" "aarch64-unknown-linux-musl" ];
          extensions = [ "clippy" "llvm-tools" ];
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

        browser-publish = pkgs.writeShellApplication {
          name = "browser-publish";
          runtimeInputs = [ pkgs.nodejs ];
          text = ''
            tmp=$(mktemp -d)
            trap 'rm -rf "$tmp"' EXIT

            cp ${browserWasm}/blit_browser.js "$tmp"/
            cp ${browserWasm}/blit_browser.d.ts "$tmp"/
            cp ${browserWasm}/blit_browser_bg.wasm "$tmp"/
            cp ${browserWasm}/blit_browser_bg.wasm.d.ts "$tmp"/ 2>/dev/null || true
            if [ -d "${browserWasm}/snippets" ]; then
              cp -r ${browserWasm}/snippets "$tmp"/snippets
            fi
            chmod -R u+w "$tmp"

            cat > "$tmp/package.json" <<'PKGJSON'
{
  "name": "blit-browser",
  "version": "${version}",
  "type": "module",
  "description": "Low-latency terminal streaming — browser WASM renderer",
  "main": "blit_browser.js",
  "types": "blit_browser.d.ts",
  "files": ["blit_browser_bg.wasm","blit_browser.js","blit_browser.d.ts","blit_browser_bg.wasm.d.ts","snippets"],
  "sideEffects": ["./snippets/*"],
  "keywords": ["terminal","tty","wasm","streaming","webgl"],
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

        installManPages = ''
          mkdir -p $out/share/man/man1
          for f in ${manPages}/share/man/man1/*.1; do
            install -m 644 "$f" $out/share/man/man1/
          done
        '';

        blit-server = rustPlatform.buildRustPackage {
          pname = "blit-server";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-server" ];
          cargoLock = cargoLockConfig;
          postInstall = installManPages;
          doCheck = false;
        };

        blit-cli = rustPlatform.buildRustPackage {
          pname = "blit-cli";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-cli" ];
          cargoLock = cargoLockConfig;
          preBuild = copyWebAppDist;
          postInstall = installManPages;
          doCheck = false;
          meta.mainProgram = "blit";
        };

        blit-gateway = rustPlatform.buildRustPackage {
          pname = "blit-gateway";
          inherit version;
          src = ./.;
          cargoBuildFlags = [ "-p" "blit-gateway" ];
          cargoLock = cargoLockConfig;
          preBuild = copyWebAppDist;
          postInstall = installManPages;
          doCheck = false;
        };

        # Static musl builds for .deb packages (Linux only).
        # pkgsStatic.makeRustPlatform handles the musl cross-compilation plumbing.
        # We supply the host (glibc) rustToolchain so cargo itself runs fine.
        # pkgsStatic.rust-bin as of 1.94.0 ships a musl-compiled cargo that
        # fails auto-patchelf due to a libgcc_s.so.1 dependency.
        # Build-script SIGSEGV fix: in the pkgsStatic env, CC is the musl compiler,
        # so without intervention cargo compiles build-scripts as musl binaries that
        # crash on the glibc build host.  CC_x86_64_unknown_linux_gnu overrides
        # the CC used for the build-host triple, giving build-scripts the glibc
        # compiler they need to run.
        rustPlatformStatic = pkgs.pkgsStatic.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        mkStaticBin = { pname, cargoPkg, extraArgs ? {} }: rustPlatformStatic.buildRustPackage ({
          inherit pname version;
          src = ./.;
          cargoBuildFlags = [ "-p" cargoPkg ];
          cargoLock = cargoLockConfig;
          doCheck = false;
        } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
          # pkgsStatic's CC wrapper setup hook sets NIX_CFLAGS_LINK=" -static",
          # which causes the glibc CC to link build scripts with -static.
          # Statically-linked glibc binaries SIGSEGV (NSS/TLS resolver init).
          # We clear it in postUnpack (after all setup hooks have run) so
          # build scripts link dynamically against glibc and run correctly.
          # The musl target is still fully static: pkgsStatic's cargo setup
          # hook writes -Ctarget-feature=+crt-static into .cargo/config.toml.
          postUnpack = "export NIX_CFLAGS_LINK=''";
        } // extraArgs);

        blit-server-static = mkStaticBin {
          pname = "blit-server";
          cargoPkg = "blit-server";
        };

        reactNpmDeps = pkgs.fetchNpmDeps {
          src = ./react;
          hash = "sha256-wDW5PEUwhHYCC9AIrHcNC6X7gO8ZAda064x+0F19jQQ=";
        };

        webAppNpmDeps = pkgs.fetchNpmDeps {
          src = ./web-app;
          hash = "sha256-LluQX9Lpmt9nlJRJRByr0HWHTa4QEoe72Wz1hAiFeeQ=";
        };

        webAppDist = pkgs.stdenv.mkDerivation {
          pname = "blit-web-app";
          inherit version;
          src = ./.;
          nativeBuildInputs = [ pkgs.nodejs ];
          buildPhase = ''
            export HOME=$TMPDIR

            # Set up browser/pkg with WASM assets
            mkdir -p browser/pkg/snippets
            cp ${browserWasm}/blit_browser.js browser/pkg/
            cp ${browserWasm}/blit_browser_bg.wasm browser/pkg/
            cp ${browserWasm}/blit_browser.d.ts browser/pkg/
            cp ${browserWasm}/blit_browser_bg.wasm.d.ts browser/pkg/
            echo '{"name":"blit-browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > browser/pkg/package.json
            for d in ${browserWasm}/snippets/blit-browser-*/; do
              name=$(basename "$d")
              mkdir -p "browser/pkg/snippets/$name"
              cp "$d"/* "browser/pkg/snippets/$name/"
            done

            # Build react package (install from prefetched cache)
            cp -r ${reactNpmDeps} "$TMPDIR/react-cache"
            chmod -R u+w "$TMPDIR/react-cache"
            (cd react && npm ci --cache "$TMPDIR/react-cache" && node node_modules/typescript/bin/tsc)

            # Build web-app (install from prefetched cache)
            # package-lock.json is committed without file: deps; vite.config.ts aliases blit-react/blit-browser to source
            cp -r ${webAppNpmDeps} "$TMPDIR/webapp-cache"
            chmod -R u+w "$TMPDIR/webapp-cache"
            (cd web-app && npm ci --cache "$TMPDIR/webapp-cache" && node node_modules/vite/bin/vite.js build)
          '';
          installPhase = ''
            mkdir -p $out
            cp web-app/dist/index.html $out/
          '';
          doCheck = false;
        };

        copyWebAppDist = ''
          mkdir -p web-app/dist
          cp ${webAppDist}/index.html web-app/dist/
        '';

        blit-cli-static = mkStaticBin {
          pname = "blit-cli";
          cargoPkg = "blit-cli";
          extraArgs = { preBuild = copyWebAppDist; };
        };

        blit-gateway-static = mkStaticBin {
          pname = "blit-gateway";
          cargoPkg = "blit-gateway";
          extraArgs = { preBuild = copyWebAppDist; };
        };


        manPagesSrc = ./man;
        manPages = pkgs.stdenv.mkDerivation {
          name = "blit-man-${version}";
          src = manPagesSrc;
          nativeBuildInputs = [ pkgs.scdoc ];
          buildPhase = ''
            mkdir -p $out/share/man/man1
            for f in *.scd; do
              scdoc < "$f" > "$out/share/man/man1/''${f%.scd}"
            done
          '';
          installPhase = "true";
        };

        mkDeb = { pname, binName ? pname, binPkg, manName ? binName, description, extraInstall ? "" }: pkgs.stdenv.mkDerivation {
          pname = "${pname}-deb";
          inherit version;
          nativeBuildInputs = [ pkgs.dpkg ];
          dontUnpack = true;
          buildPhase =
            let arch = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
            in ''
              mkdir -p pkg/DEBIAN pkg/usr/bin pkg/usr/share/man/man1
              cp ${binPkg}/bin/${binName} pkg/usr/bin/
              if [ -f ${manPages}/share/man/man1/${manName}.1 ]; then
                cp ${manPages}/share/man/man1/${manName}.1 pkg/usr/share/man/man1/
                gzip -9 pkg/usr/share/man/man1/${manName}.1
              fi
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
        packages.browser-publish = browser-publish;
        packages.react-publish = react-publish;
        packages.default = blit-cli;

        packages.blit-server-static = blit-server-static;
        packages.blit-cli-static = blit-cli-static;
        packages.blit-gateway-static = blit-gateway-static;

        packages.blit-server-deb = mkDeb {
          pname = "blit-server";
          binPkg = blit-server-static;
          description = "blit terminal streaming server";
          extraInstall = let
            systemdDir = ./systemd;
          in ''
            mkdir -p pkg/lib/systemd/system
            cp "${systemdDir}/blit@.socket" "pkg/lib/systemd/system/blit@.socket"
            cp "${systemdDir}/blit@.service" "pkg/lib/systemd/system/blit@.service"
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

        packages.e2e = pkgs.writeShellApplication {
          name = "blit-e2e";
          runtimeInputs = [ pkgs.nodejs pkgs.pnpm ];
          text = ''
            export PLAYWRIGHT_BROWSERS_PATH="${pkgs.playwright-driver.browsers}"
            export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1

            echo "=== Setting up binaries ==="
            mkdir -p target/debug
            ln -sf "${blit-server}/bin/blit-server" target/debug/blit-server
            ln -sf "${blit-gateway}/bin/blit-gateway" target/debug/blit-gateway

            echo "=== Installing e2e deps ==="
            (cd e2e && if ! pnpm install --frozen-lockfile 2>/dev/null; then pnpm install; fi)

            echo "=== Running Playwright ==="
            (cd e2e && npx playwright test)
          '';
        };

        packages.lint = pkgs.writeShellApplication {
          name = "blit-lint";
          runtimeInputs = [ rustToolchain ];
          text = ''
            echo "=== Setting up web-app dist ==="
            mkdir -p web-app/dist
            cp ${webAppDist}/index.html web-app/dist/

            echo "=== Clippy ==="
            cargo clippy --workspace -- -D warnings
          '';
        };

        packages.tests = pkgs.writeShellApplication {
          name = "blit-tests";
          runtimeInputs = [ rustToolchain pkgs.nodejs pkgs.pnpm ];
          text = ''
            echo "=== Setting up web-app dist ==="
            mkdir -p web-app/dist
            cp ${webAppDist}/index.html web-app/dist/

            echo "=== Rust tests ==="
            cargo test --workspace
            echo ""
            echo "=== React tests ==="
            mkdir -p browser/pkg
            if [ ! -f browser/pkg/package.json ]; then
              echo '{"name":"blit-browser","version":"0.0.0","main":"blit_browser.js"}' > browser/pkg/package.json
            fi
            if [ ! -f browser/pkg/blit_browser.js ]; then
              touch browser/pkg/blit_browser.js
            fi
            (cd react && { pnpm install --frozen-lockfile 2>/dev/null || pnpm install; } && pnpm vitest run)
          '';
        };

        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.binaryen
            pkgs.bun
            pkgs.cargo-flamegraph
            pkgs.cargo-llvm-cov
            pkgs.cargo-edit
            pkgs.cargo-watch
            pkgs.curl
            pkgs.nodejs
            pkgs.pkgsStatic.stdenv.cc
            pkgs.pnpm
            pkgs.prefetch-npm-deps
            pkgs.process-compose
            pkgs.samply
            pkgs.scdoc
            pkgs.wasm-bindgen-cli
            pkgs.wasm-pack
          ];

          shellHook = ''
            if [ -z "''${LANG-}" ]; then
              export LANG="$(defaults read -g AppleLocale 2>/dev/null | sed 's/@.*//' || echo en_US).UTF-8"
            fi
            echo "blit dev shell"
            echo "  dev:                dev  (server + gateway + browser assets, auto-reload on source change)"
            echo "  build:              build"
            echo "  run server:         cargo run -p blit-server"
            echo "  run gateway:        BLIT_PASS=secret cargo run -p blit-gateway  # http://localhost:3264"
            echo "  run cli:            cargo run -p blit-cli"
            echo "  flamegraph:         flamegraph -- target/release/blit-server"
          '';
        };
      }
    );
}
