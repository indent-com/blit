{ inputs, ... }:
{
  perSystem =
    { system, ... }:
    let
      common = import ./common.nix { inherit inputs system; };
      inherit (common)
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
      serverVaapiEnabled = pkgs.stdenv.isLinux;
      bindgenClangArgs = pkgs.lib.optionalString pkgs.stdenv.isLinux "-isystem ${pkgs.lib.getDev pkgs.stdenv.cc.libc}/include";

      # Runtime library search path for blit server's dlopen GPU backends.
      #   pkgs.libva           → libva.so.2, libva-drm.so.2
      #   pkgs.libgbm          → libgbm.so.1
      #   pkgs.vulkan-loader   → libvulkan.so.1 (Vulkan dispatch)
      #   addDriverRunpath     → /run/opengl-driver  (libcuda, libnvidia-encode,
      #                          Mesa VA-API / Vulkan drivers, etc.)
      gpuRuntimeLibPath = pkgs.lib.optionalString serverVaapiEnabled (
        pkgs.lib.makeLibraryPath [
          pkgs.libva
          pkgs.libgbm
          pkgs.vulkan-loader
          pkgs.addDriverRunpath.driverLink
        ]
      );

      # ------------------------------------------------------------------
      # Crane build
      # ------------------------------------------------------------------

      blit = craneLib.buildPackage (
        commonArgs
        // {
          pname = "blit";
          inherit cargoArtifacts;
          cargoExtraArgs = "-p blit-cli";
          doCheck = false;
          preBuild = copyWebAppDist;
          postInstall = ''
            $out/bin/blit generate $out/share
          '';
          meta.mainProgram = "blit";
        }
      );

      # ------------------------------------------------------------------
      # WASM (still uses wasm-pack, not crane)
      # ------------------------------------------------------------------

      browserWasm = rustPlatform.buildRustPackage {
        pname = "blit-browser";
        inherit version;
        src = ../.;
        cargoBuildFlags = [
          "-p"
          "blit-browser"
        ];
        cargoLock = cargoLockConfig;
        nativeBuildInputs = [
          pkgs.wasm-pack
          pkgs.wasm-bindgen-cli
          pkgs.binaryen
        ];
        buildPhase = ''
          cd crates/browser
          HOME=$TMPDIR wasm-pack build --target web --release --out-dir $out
        '';
        dontInstall = true;
        doCheck = false;
      };

      # ------------------------------------------------------------------
      # Release binaries
      #
      # Linux ships two variants:
      #   glibc (blit-gnu)  — dynamically linked, dlopen works for GPU
      #   musl  (blit-musl) — all deps statically linked except musl libc
      # macOS ships a single binary with nix-store dylibs rewritten to
      # system paths.
      # ------------------------------------------------------------------

      # Linux glibc binary — all deps statically linked, only glibc is
      # dynamic (so dlopen works for GPU).  Built with cargo-zigbuild
      # targeting glibc ${minGlibcVersion} for broad distro compat.
      blit-gnu = craneLib.buildPackage (
        commonArgsGnu
        // {
          pname = "blit-gnu";
          cargoArtifacts = cargoArtifactsGnu;
          cargoExtraArgs = "-p blit-cli";
          doCheck = false;
          preBuild = copyWebAppDist;
          buildPhaseCargoCommand = "HOME=$TMPDIR cargo zigbuild --release --target ${rustTargetGnu}.${minGlibcVersion} -p blit-cli";
          doNotPostBuildInstallCargoBinaries = true;
          installPhaseCommand = ''
            mkdir -p $out/bin
            cp target/${rustTargetGnu}/release/blit $out/bin/
          '';
        }
      );

      # Linux musl dynamic binary — all deps statically linked except
      # musl libc.  For Alpine and other musl-based systems.
      blit-musl = craneLibStatic.buildPackage (
        commonArgsStatic
        // {
          pname = "blit-musl";
          cargoArtifacts = cargoArtifactsStatic;
          cargoExtraArgs = "-p blit-cli";
          doCheck = false;
          preBuild = copyWebAppDist;
          dontPatchELF = true;
          postFixup = pkgs.lib.optionalString pkgs.stdenv.isLinux ''
            for bin in $out/bin/*; do
              interp=$(readelf -l "$bin" 2>/dev/null \
                | grep -oP 'Requesting program interpreter: \K[^\]]+' || true)
              case "$(basename "$interp")" in
                ld-musl-*) ;;
                *) echo "FATAL: expected musl interpreter, got: $interp"; exit 1 ;;
              esac
              needed=$(readelf -d "$bin" 2>/dev/null \
                | grep -oP '\(NEEDED\)\s+Shared library: \[\K[^\]]+' || true)
              for lib in $needed; do
                case "$lib" in
                  libc.so) ;;
                  *) echo "FATAL: unexpected NEEDED library: $lib"; exit 1 ;;
                esac
              done
            done
          '';
        }
      );

      # Assembled glibc release: single binary with system interpreter.
      # All deps are statically linked; only glibc is dynamic.
      blit-release-gnu =
        let
          interpreter =
            if pkgs.stdenv.hostPlatform.isAarch64
            then "/lib/ld-linux-aarch64.so.1"
            else "/lib64/ld-linux-x86_64.so.2";
        in
        pkgs.runCommand "blit-release-gnu-${version}" {
          nativeBuildInputs = [ pkgs.patchelf ];
        } ''
          mkdir -p $out/bin
          cp ${blit-gnu}/bin/blit $out/bin/blit
          chmod +w $out/bin/blit
          patchelf --set-interpreter ${interpreter} $out/bin/blit
          patchelf --remove-rpath $out/bin/blit
        '';

      # Assembled musl release: single binary, interpreter set to
      # system musl path.
      blit-release-musl =
        let
          arch = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
        in
        pkgs.runCommand "blit-release-musl-${version}" {
          nativeBuildInputs = [ pkgs.patchelf ];
        } ''
          mkdir -p $out/bin
          cp ${blit-musl}/bin/blit $out/bin/blit
          chmod +w $out/bin/blit
          patchelf --set-interpreter /lib/ld-musl-${arch}.so.1 $out/bin/blit
        '';

      # Default release package per platform.
      blit-release =
        if pkgs.stdenv.isLinux then
          blit-release-gnu
        else
          # macOS: rewrite nix-store dylibs to system paths.
          craneLibStatic.buildPackage (
            commonArgsStatic
            // {
              pname = "blit-release";
              cargoArtifacts = cargoArtifactsStatic;
              cargoExtraArgs = "-p blit-cli";
              doCheck = false;
              preBuild = copyWebAppDist;
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
            }
          );


      # ------------------------------------------------------------------
      # JS / Web assets
      # ------------------------------------------------------------------

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

      pnpmDeps = pkgs.fetchPnpmDeps {
        pname = "blit-js";
        inherit version;
        src = ../.;
        fetcherVersion = 3;
        postPatch = setupBrowserPkg + ''
          cd js
        '';
        hash = "sha256-wyMe+IE6XcyVnTkvYi6tk+0xXLTlYnl7QEuagGS789k=";
      };

      webAppDist = pkgs.stdenv.mkDerivation {
        pname = "blit-ui";
        inherit version;
        src = ../.;
        inherit pnpmDeps;
        nativeBuildInputs = [
          pkgs.nodejs
          pkgs.pnpm
          pkgs.pnpmConfigHook
        ];
        pnpmRoot = "js";
        postPatch = setupBrowserPkg;
        buildPhase = ''
          cd js
          pnpm --filter @blit-sh/core run build
          pnpm --filter @blit-sh/solid run build
          pnpm --filter @blit-sh/ui run build
        '';
        installPhase = ''
          mkdir -p $out
          cp ui/dist/index.html ui/dist/index.html.br $out/
        '';
        doCheck = false;
      };

      websiteDist = pkgs.stdenv.mkDerivation {
        pname = "blit-website";
        inherit version;
        src = ../.;
        inherit pnpmDeps;
        nativeBuildInputs = [
          pkgs.nodejs
          pkgs.pnpm
          pkgs.pnpmConfigHook
        ];
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
        mkdir -p js/ui/dist
        cp ${webAppDist}/index.html ${webAppDist}/index.html.br js/ui/dist/
      '';

      # Man pages and shell completions are generated by `blit generate`
      # during postInstall.

      # ------------------------------------------------------------------
      # Docker / tasks
      # ------------------------------------------------------------------

      tasks = import ./tasks.nix {
        inherit
          pkgs
          version
          browserWasm
          blit
          blit-release
          webAppDist
          websiteDist
          rustToolchain
          ;
        blit-release-musl = if pkgs.stdenv.isLinux then blit-release-musl else null;
      };

      demoImage =
        let
          fishConfig = pkgs.writeTextDir "home/blit/.config/fish/config.fish" ''
            function fish_greeting
                cat /etc/blit-welcome 2>/dev/null
            end
          '';
          welcomeFile = pkgs.writeTextDir "etc/blit-welcome" (
            if builtins.pathExists ../welcome then builtins.readFile ../welcome else ""
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
            pkgs.mpv
            pkgs.imv
            pkgs.wayland-utils
            pkgs.foot
            pkgs.wev
            pkgs.zathura
            blit
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
            ExposedPorts = {
              "3264/tcp" = { };
            };
            Entrypoint = [
              "blit"
              "share"
            ];
          };
        };

      skopeoPolicy = pkgs.writeText "containers-policy.json" ''{"default":[{"type":"insecureAcceptAnything"}]}'';

      pushDemo = pkgs.writeShellApplication {
        name = "push-demo";
        runtimeInputs = [ pkgs.skopeo ];
        text = ''
          arch="''${1:?usage: push-demo <amd64|arm64> [version]}"
          version="''${2:-}"
          creds="$DOCKERHUB_USERNAME:$DOCKERHUB_TOKEN"
          skopeo --policy ${skopeoPolicy} copy --dest-creds "$creds" "docker-archive:${demoImage}" "docker://docker.io/grab/blit-demo:latest-$arch"
          if [[ "$version" != "" ]]; then
            skopeo --policy ${skopeoPolicy} copy --dest-creds "$creds" "docker-archive:${demoImage}" "docker://docker.io/grab/blit-demo:$version-$arch"
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
        inherit
          blit
          blit-release
          ;
        demo-image = demoImage;
        push-demo = pushDemo;
        publish-demo = publishDemo;
        default = blit;
      }
      // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
        inherit blit-release-musl;
      }
      // tasks;

      devShells.default = pkgs.mkShell {
        buildInputs = [
          rustToolchain
          pkgs.binaryen
          pkgs.bun
          pkgs.cargo-flamegraph
          pkgs.cargo-llvm-cov
          pkgs.cargo-edit
          pkgs.cmake
          pkgs.cargo-watch
          pkgs.curl
          pkgs.flyctl
          pkgs.libopus
          pkgs.nodejs
          pkgs.pkg-config
          pkgs.pkgsStatic.stdenv.cc
          pkgs.pnpm
          pkgs.process-compose
          pkgs.samply
          pkgs.socat
          pkgs.wasm-bindgen-cli
          pkgs.wasm-pack
        ]
        ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          pkgs.dbus
          pkgs.pipewire
          pkgs.wireplumber
          pkgs.llvmPackages.libclang
        ];

        shellHook = ''
          if [ -z "''${LANG-}" ]; then
            export LANG="$(defaults read -g AppleLocale 2>/dev/null | sed 's/@.*//' || echo en_US).UTF-8"
          fi
          export BINDGEN_EXTRA_CLANG_ARGS="${bindgenClangArgs}''${NIX_CFLAGS_COMPILE:+ $NIX_CFLAGS_COMPILE}"
          export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
          export PKG_CONFIG_PATH="${pkgs.libopus.dev}/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
          export LIBRARY_PATH="${pkgs.libopus}/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}"
          # Runtime dlopen: blit server loads VA-API / NVENC libraries at
          # runtime via dlopen.  See gpuRuntimeLibPath definition above.
          ${pkgs.lib.optionalString (
            gpuRuntimeLibPath != ""
          ) ''export LD_LIBRARY_PATH="${gpuRuntimeLibPath}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"''}
          export PATH="$PWD/target/profiling:$PWD/bin:$PATH"
        '';
      };
    };
}
