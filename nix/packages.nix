{ inputs, ... }: {
  perSystem = { system, ... }:
    let
      common = import ./common.nix { inherit inputs system; };
      inherit (common) pkgs version cargoLockConfig rustToolchain rustPlatform;

      browserWasm = rustPlatform.buildRustPackage {
        pname = "blit-browser";
        inherit version;
        src = ../.;
        cargoBuildFlags = [ "-p" "blit-browser" ];
        cargoLock = cargoLockConfig;
        nativeBuildInputs = [ pkgs.wasm-pack pkgs.wasm-bindgen-cli pkgs.binaryen ];
        buildPhase = ''
          cd crates/browser
          HOME=$TMPDIR wasm-pack build --target web --release --out-dir $out
        '';
        dontInstall = true;
        doCheck = false;
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

      blit-webrtc-forwarder = rustPlatform.buildRustPackage {
        pname = "blit-webrtc-forwarder";
        inherit version;
        src = ../.;
        cargoBuildFlags = [ "-p" "blit-webrtc-forwarder" ];
        cargoLock = cargoLockConfig;
        doCheck = false;
      };

      blit-webrtc-forwarder-static = mkStaticBin {
        pname = "blit-webrtc-forwarder";
        cargoPkg = "blit-webrtc-forwarder";
      };

      reactNpmDeps = pkgs.fetchNpmDeps {
        src = ../js/react;
        hash = "sha256-myDI5SaAfx2kpEaEdYv/BSiUTge0T48kkMyUZ1t3zRQ=";
      };

      webAppNpmDeps = pkgs.fetchNpmDeps {
        src = ../js/web-app;
        hash = "sha256-OgNos+GJ3tf5N3JZlygEdbAz2a6LQLoiBd87n29zYPg=";
      };

      webAppDist = pkgs.stdenv.mkDerivation {
        pname = "blit-web-app";
        inherit version;
        src = ../.;
        nativeBuildInputs = [ pkgs.nodejs ];
        buildPhase = ''
          export HOME=$TMPDIR

          mkdir -p crates/browser/pkg/snippets
          cp ${browserWasm}/blit_browser.js crates/browser/pkg/
          cp ${browserWasm}/blit_browser_bg.wasm crates/browser/pkg/
          cp ${browserWasm}/blit_browser.d.ts crates/browser/pkg/
          cp ${browserWasm}/blit_browser_bg.wasm.d.ts crates/browser/pkg/
          echo '{"name":"blit-browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > crates/browser/pkg/package.json
          for d in ${browserWasm}/snippets/blit-browser-*/; do
            name=$(basename "$d")
            mkdir -p "crates/browser/pkg/snippets/$name"
            cp "$d"/* "crates/browser/pkg/snippets/$name/"
          done

          cp -r ${reactNpmDeps} "$TMPDIR/react-cache"
          chmod -R u+w "$TMPDIR/react-cache"
          (cd js/react && npm ci --cache "$TMPDIR/react-cache" && node node_modules/typescript/bin/tsc)

          cp -r ${webAppNpmDeps} "$TMPDIR/webapp-cache"
          chmod -R u+w "$TMPDIR/webapp-cache"
          (cd js/web-app && npm ci --cache "$TMPDIR/webapp-cache" && node node_modules/vite/bin/vite.js build)
        '';
        installPhase = ''
          mkdir -p $out
          cp js/web-app/dist/index.html $out/
        '';
        doCheck = false;
      };

      copyWebAppDist = ''
        mkdir -p js/web-app/dist
        cp ${webAppDist}/index.html js/web-app/dist/
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

      manPages = pkgs.stdenv.mkDerivation {
        name = "blit-man-${version}";
        src = ../man;
        nativeBuildInputs = [ pkgs.scdoc ];
        buildPhase = ''
          mkdir -p $out/share/man/man1
          for f in *.scd; do
            scdoc < "$f" > "$out/share/man/man1/''${f%.scd}"
          done
        '';
        installPhase = "true";
      };

      tasks = import ./tasks.nix {
        inherit pkgs version browserWasm blit-server blit-gateway
                blit-server-static blit-cli-static blit-gateway-static
                blit-webrtc-forwarder-static
                manPages webAppDist rustToolchain;
      };
    in
    {
      packages = {
        blit = blit-cli;
        inherit blit-server blit-cli blit-gateway blit-webrtc-forwarder;
        inherit blit-server-static blit-cli-static blit-gateway-static blit-webrtc-forwarder-static;
        default = blit-cli;
      } // tasks;

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
          pkgs.flyctl
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
