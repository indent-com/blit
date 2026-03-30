{ inputs, ... }: {
  perSystem = { system, ... }:
    let
      pkgs = import inputs.nixpkgs {
        inherit system;
        overlays = [ inputs.rust-overlay.overlays.default ];
      };

      version = "0.11.2";

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

      browserWasm = rustPlatform.buildRustPackage {
        pname = "blit-browser";
        inherit version;
        src = ../.;
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
  "homepage": "https://blit.sh",
  "license": "MIT",
  "author": "Indent <oss@indent.com> (https://indent.com)",
  "repository": {"type":"git","url":"git+https://github.com/indent-com/blit.git","directory":"browser"},
  "bugs": {"url":"https://github.com/indent-com/blit/issues"}
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
          cp -a ${../react}/* "$tmp"/
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
        src = ../.;
        cargoBuildFlags = [ "-p" "blit-server" ];
        cargoLock = cargoLockConfig;
        postInstall = installManPages;
        doCheck = false;
      };

      blit-cli = rustPlatform.buildRustPackage {
        pname = "blit-cli";
        inherit version;
        src = ../.;
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
        src = ../.;
        cargoBuildFlags = [ "-p" "blit-gateway" ];
        cargoLock = cargoLockConfig;
        preBuild = copyWebAppDist;
        postInstall = installManPages;
        doCheck = false;
      };

      rustPlatformStatic = pkgs.pkgsStatic.makeRustPlatform {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      mkStaticBin = { pname, cargoPkg, extraArgs ? {} }: rustPlatformStatic.buildRustPackage ({
        inherit pname version;
        src = ../.;
        cargoBuildFlags = [ "-p" cargoPkg ];
        cargoLock = cargoLockConfig;
        doCheck = false;
      } // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
        postUnpack = "export NIX_CFLAGS_LINK=''";
        postFixup = ''
          for bin in $out/bin/*; do
            if ! file "$bin" | grep -qE "static(ally|-pie) linked"; then
              echo "FATAL: $bin is not statically linked:"
              file "$bin"
              exit 1
            fi
          done
        '';
      } // pkgs.lib.optionalAttrs pkgs.stdenv.isDarwin {
        postFixup = ''
          for bin in $out/bin/*; do
            for lib in $(otool -L "$bin" | tail -n +2 | awk '/\/nix\/store\//{print $1}'); do
              base=$(basename "$lib")
              case "$base" in
                libiconv.*|libiconv-*) sys="/usr/lib/libiconv.2.dylib" ;;
                libz.*|libz-*) sys="/usr/lib/libz.1.dylib" ;;
                libc++.*) sys="/usr/lib/libc++.1.dylib" ;;
                libc++abi.*) sys="/usr/lib/libc++abi.dylib" ;;
                libresolv.*) sys="/usr/lib/libresolv.9.dylib" ;;
                libSystem.*) sys="/usr/lib/libSystem.B.dylib" ;;
                *) echo "FATAL: unknown nix-store dylib: $lib"; exit 1 ;;
              esac
              echo "rewriting $lib -> $sys"
              install_name_tool -change "$lib" "$sys" "$bin"
            done
          done
        '';
      } // extraArgs);

      blit-server-static = mkStaticBin {
        pname = "blit-server";
        cargoPkg = "blit-server";
      };

      reactNpmDeps = pkgs.fetchNpmDeps {
        src = ../react;
        hash = "sha256-NGLyzd6zivzpB3+Vm9Y4YNNRLVeCHOv4axPqk5Hi3Uk=";
      };

      webAppNpmDeps = pkgs.fetchNpmDeps {
        src = ../web-app;
        hash = "sha256-LluQX9Lpmt9nlJRJRByr0HWHTa4QEoe72Wz1hAiFeeQ=";
      };

      webAppDist = pkgs.stdenv.mkDerivation {
        pname = "blit-web-app";
        inherit version;
        src = ../.;
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

      manPagesSrc = ../man;
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
      blit-server-deb = mkDeb {
        pname = "blit-server";
        binPkg = blit-server-static;
        description = "blit terminal streaming server";
        extraInstall = let
          systemdDir = ../systemd;
        in ''
          mkdir -p pkg/lib/systemd/system
          cp "${systemdDir}/blit@.socket" "pkg/lib/systemd/system/blit@.socket"
          cp "${systemdDir}/blit@.service" "pkg/lib/systemd/system/blit@.service"
        '';
      };
      blit-cli-deb = mkDeb {
        pname = "blit";
        binPkg = blit-cli-static;
        description = "blit terminal client";
      };
      blit-gateway-deb = mkDeb {
        pname = "blit-gateway";
        binPkg = blit-gateway-static;
        description = "blit WebSocket gateway";
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

      packages.blit-server-deb = blit-server-deb;
      packages.blit-cli-deb = blit-cli-deb;
      packages.blit-gateway-deb = blit-gateway-deb;

      packages.build-debs = pkgs.writeShellApplication {
        name = "blit-build-debs";
        text = ''
          outdir="''${1:-dist/debs}"
          mkdir -p "$outdir"
          for pkg in ${blit-server-deb} ${blit-cli-deb} ${blit-gateway-deb}; do
            cp "$pkg"/*.deb "$outdir"/
          done
          echo ""
          ls -lh "$outdir"
        '';
      };

      packages.build-tarballs = pkgs.writeShellApplication {
        name = "blit-build-tarballs";
        text = let
          os = if pkgs.stdenv.isDarwin then "darwin" else "linux";
          arch = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
        in ''
          outdir="''${1:-dist/tarballs}"
          mkdir -p "$outdir"
          for pkg in ${blit-server-static} ${blit-cli-static} ${blit-gateway-static}; do
            for bin in "$pkg"/bin/*; do
              name=$(basename "$bin")
              tar -czf "$outdir/''${name}_${version}_${os}_${arch}.tar.gz" -C "$pkg/bin" "$name"
            done
          done
          echo ""
          ls -lh "$outdir"
        '';
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
    };
}
