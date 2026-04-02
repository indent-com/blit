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

      # Helper to set up WASM browser pkg for JS builds.
      setupBrowserPkg = ''
        mkdir -p crates/browser/pkg/snippets
        cp ${browserWasm}/blit_browser.js crates/browser/pkg/
        cp ${browserWasm}/blit_browser_bg.wasm crates/browser/pkg/
        cp ${browserWasm}/blit_browser.d.ts crates/browser/pkg/
        cp ${browserWasm}/blit_browser_bg.wasm.d.ts crates/browser/pkg/
        echo '{"name":"@blit-sh/browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > crates/browser/pkg/package.json
        for d in ${browserWasm}/snippets/blit-browser-*/; do
          name=$(basename "$d")
          mkdir -p "crates/browser/pkg/snippets/$name"
          cp "$d"/* "crates/browser/pkg/snippets/$name/"
        done
      '';

      # Pre-fetch pnpm dependencies in a fixed-output derivation (has network
      # access). The resulting store is used offline by webAppDist/websiteDist.
      pnpmDeps = pkgs.fetchPnpmDeps {
        pname = "blit-js";
        inherit version;
        src = ../.;
        fetcherVersion = 3;
        postPatch = setupBrowserPkg + ''
          cd js
        '';
        hash = "sha256-ocPLa+jPUzn9SfsVtIslo3Unbku2RA6sQroI9i1YJ3k=";
      };

      webAppDist = pkgs.stdenv.mkDerivation {
        pname = "blit-web-app";
        inherit version;
        src = ../.;
        inherit pnpmDeps;
        nativeBuildInputs = [ pkgs.nodejs pkgs.pnpm pkgs.pnpmConfigHook ];
        pnpmRoot = "js";
        postPatch = setupBrowserPkg;
        buildPhase = ''
          cd js
          pnpm --filter @blit-sh/core run build
          pnpm --filter @blit-sh/react run build
          pnpm --filter blit-web-app run build
        '';
        installPhase = ''
          mkdir -p $out
          cp web-app/dist/index.html $out/
        '';
        doCheck = false;
      };

      websiteDist = pkgs.stdenv.mkDerivation {
        pname = "blit-website";
        inherit version;
        src = ../.;
        inherit pnpmDeps;
        nativeBuildInputs = [ pkgs.nodejs pkgs.pnpm pkgs.pnpmConfigHook ];
        pnpmRoot = "js";
        postPatch = setupBrowserPkg;
        buildPhase = ''
          cd js
          pnpm --filter blit-website run build
        '';
        installPhase = ''
          mkdir -p $out
          cp -r website/dist/* $out/
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
                manPages webAppDist websiteDist rustToolchain;
      };

      demoImage = let
        fishConfig = pkgs.writeTextDir "home/blit/.config/fish/config.fish" ''
          function fish_greeting
              cat /etc/blit-welcome 2>/dev/null
          end
        '';
        welcomeFile = pkgs.writeTextDir "etc/blit-welcome" (
          if builtins.pathExists ../welcome
          then builtins.readFile ../welcome
          else ""
        );
        passwd = pkgs.writeTextDir "etc/passwd" "blit:x:1000:1000:blit:/home/blit:/bin/fish\n";
        group = pkgs.writeTextDir "etc/group" "blit:x:1000:\n";
      in
      pkgs.dockerTools.buildLayeredImage {
        name = "grab/blit-demo";
        tag = "latest";
        maxLayers = 2;
        contents = [
          pkgs.dockerTools.caCertificates
          pkgs.dockerTools.binSh
          pkgs.busybox
          pkgs.fish
          pkgs.htop
          pkgs.neovim
          pkgs.git
          pkgs.curl
          pkgs.jq
          pkgs.tree
          pkgs.ncdu
          blit-cli
          fishConfig
          welcomeFile
          passwd
          group
        ];
        fakeRootCommands = ''
          mkdir -p ./home/blit ./tmp
          chown -R 1000:1000 ./home/blit
          chmod 1777 ./tmp
        '';
        config = {
          Env = [
            "SHELL=/bin/fish"
            "USER=blit"
            "HOME=/home/blit"
            "TERM=xterm-256color"
          ];
          User = "1000:1000";
          WorkingDir = "/home/blit";
          ExposedPorts = { "3264/tcp" = {}; };
          Entrypoint = [ "blit" "share" ];
        };
      };

      skopeoPolicy = pkgs.writeText "containers-policy.json" ''{"default":[{"type":"insecureAcceptAnything"}]}'';

      pushDemo = pkgs.writeShellApplication {
        name = "push-demo";
        runtimeInputs = [ pkgs.skopeo ];
        text = ''
          arch="''${1:?usage: push-demo <amd64|arm64> [version]}"
          version="''${2:-}"
          skopeo --policy ${skopeoPolicy} login docker.io -u "$DOCKERHUB_USERNAME" -p "$DOCKERHUB_TOKEN"
          skopeo --policy ${skopeoPolicy} copy "docker-archive:${demoImage}" "docker://docker.io/grab/blit-demo:latest-$arch"
          if [[ "$version" != "" ]]; then
            skopeo --policy ${skopeoPolicy} copy "docker-archive:${demoImage}" "docker://docker.io/grab/blit-demo:$version-$arch"
          fi
        '';
      };

      publishDemo = pkgs.writeShellApplication {
        name = "publish-demo";
        runtimeInputs = [ pkgs.crane ];
        text = ''
          version="''${1:-}"
          crane auth login docker.io -u "$DOCKERHUB_USERNAME" -p "$DOCKERHUB_TOKEN"
          crane index append \
            -t "docker.io/grab/blit-demo:latest" \
            -m "docker.io/grab/blit-demo:latest-amd64" \
            -m "docker.io/grab/blit-demo:latest-arm64"
          if [[ "$version" != "" ]]; then
            crane index append \
              -t "docker.io/grab/blit-demo:$version" \
              -m "docker.io/grab/blit-demo:$version-amd64" \
              -m "docker.io/grab/blit-demo:$version-arm64"
          fi
        '';
      };
    in
    {
      packages = {
        blit = blit-cli;
        inherit blit-server blit-cli blit-gateway blit-webrtc-forwarder;
        inherit blit-server-static blit-cli-static blit-gateway-static blit-webrtc-forwarder-static;
        demo-image = demoImage;
        push-demo = pushDemo;
        publish-demo = publishDemo;
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
