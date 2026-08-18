{
  pkgs,
  version,
  browserWasm,
  browserWasmNode,
  blit,
  blit-release,
  blit-release-musl ? null,
  blit-release-gnu-gpl ? null,
  blit-release-musl-gpl ? null,
  webAppDist,
  websiteDist,
  rustToolchain,
}:
let
  # Helper to set up WASM browser pkg for JS builds.
  setupBrowserPkg = ''
    mkdir -p crates/browser/pkg/snippets
    cp ${browserWasm}/blit_browser.js ${browserWasm}/blit_browser.d.ts crates/browser/pkg/
    cp ${browserWasm}/blit_browser_bg.wasm crates/browser/pkg/
    cp ${browserWasm}/blit_browser_bg.wasm.d.ts crates/browser/pkg/ 2>/dev/null || true
    # An explicit `files` list is required, not cosmetic: crates/browser/pkg
    # is gitignored, and with no `files` and no .npmignore, npm/pnpm packing
    # falls back to .gitignore and drops everything but package.json and
    # main — shipping the package without its .d.ts, which fails the
    # js/website typecheck on a clean checkout.
    echo '{"name":"@blit-sh/browser","version":"${version}","files":["blit_browser.js","blit_browser.d.ts","blit_browser_bg.wasm","blit_browser_bg.wasm.d.ts","snippets"],"main":"blit_browser.js","types":"blit_browser.d.ts"}' > crates/browser/pkg/package.json
    if [ -d "${browserWasm}/snippets" ]; then
      for d in ${browserWasm}/snippets/blit-browser-*/; do
        name=$(basename "$d")
        mkdir -p "crates/browser/pkg/snippets/$name"
        cp "$d"/* "crates/browser/pkg/snippets/$name/"
      done
    fi
  '';

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

            # Self-initializing Node/Bun build under ./node (see
            # nix/packages.nix `browserWasmNode`).  Exposed via the
            # `@blit-sh/browser/node` subpath; the root export stays the
            # `--target web` build so existing browser consumers are unaffected.
            mkdir -p "$tmp/node"
            cp ${browserWasmNode}/blit_browser.js "$tmp/node"/
            cp ${browserWasmNode}/blit_browser.d.ts "$tmp/node"/
            cp ${browserWasmNode}/blit_browser_bg.wasm "$tmp/node"/
            cp ${browserWasmNode}/blit_browser_bg.wasm.d.ts "$tmp/node"/ 2>/dev/null || true
            if [ -d "${browserWasmNode}/snippets" ]; then
              cp -r ${browserWasmNode}/snippets "$tmp/node"/snippets
            fi
            cp ${browserWasmNode}/package.json "$tmp/node"/package.json
            chmod -R u+w "$tmp/node"

            cat > "$tmp/package.json" <<'PKGJSON'
      {
        "name": "@blit-sh/browser",
        "version": "${version}",
        "type": "module",
        "description": "Low-latency terminal streaming — browser WASM renderer",
        "main": "blit_browser.js",
        "types": "blit_browser.d.ts",
        "exports": {
          ".": { "types": "./blit_browser.d.ts", "default": "./blit_browser.js" },
          "./node": { "types": "./node/blit_browser.d.ts", "default": "./node/blit_browser.js" },
          "./blit_browser.js": "./blit_browser.js",
          "./blit_browser_bg.wasm": "./blit_browser_bg.wasm",
          "./blit_browser_bg.wasm.d.ts": "./blit_browser_bg.wasm.d.ts",
          "./snippets/*": "./snippets/*",
          "./package.json": "./package.json"
        },
        "files": ["blit_browser_bg.wasm","blit_browser.js","blit_browser.d.ts","blit_browser_bg.wasm.d.ts","snippets","node"],
        "sideEffects": ["./snippets/*"],
        "keywords": ["terminal","tty","wasm","streaming","webgl"],
        "homepage": "https://blit.sh",
        "license": "MIT",
        "author": "Indent <oss@indent.com> (https://indent.com)",
        "repository": {"type":"git","url":"git+https://github.com/indent-com/blit.git","directory":"crates/browser"},
        "bugs": {"url":"https://github.com/indent-com/blit/issues"}
      }
      PKGJSON
            echo "Package contents:"
            ls -lh "$tmp"
            echo ""
            npm publish "$tmp" "$@"
    '';
  };

  # Publish @blit-sh/core, @blit-sh/react, @blit-sh/solid using the pnpm workspace.
  js-publish = pkgs.writeShellApplication {
    name = "js-publish";
    runtimeInputs = [
      pkgs.nodejs
      pkgs.pnpm
    ];
    text = ''
      pkg_name="$1"
      shift

      tmp=$(mktemp -d)
      trap 'rm -rf "$tmp"' EXIT

      cp -a ${../.}/* "$tmp"/
      chmod -R u+w "$tmp"

      cd "$tmp"
      ${setupBrowserPkg}

      cd js
      pnpm install --frozen-lockfile
      pnpm --filter "$pkg_name" run build

      # pnpm publish resolves workspace:* to real versions
      pnpm --filter "$pkg_name" publish --no-git-checks "$@"
    '';
  };

  publish-npm-packages = pkgs.writeShellApplication {
    name = "blit-publish-npm-packages";
    runtimeInputs = [
      pkgs.nodejs
      pkgs.pnpm
    ];
    text = ''
      echo "=== Publishing @blit-sh/browser ==="
      ${browser-publish}/bin/browser-publish "$@"
      echo ""
      echo "=== Publishing @blit-sh/core ==="
      ${js-publish}/bin/js-publish @blit-sh/core "$@"
      echo ""
      echo "=== Publishing @blit-sh/react ==="
      ${js-publish}/bin/js-publish @blit-sh/react "$@"
      echo ""
      echo "=== Publishing @blit-sh/solid ==="
      ${js-publish}/bin/js-publish @blit-sh/solid "$@"
    '';
  };

  mkDeb =
    {
      pname,
      binName ? pname,
      binPkg,
      description,
      extraInstall ? "",
    }:
    pkgs.stdenv.mkDerivation {
      pname = "${pname}-deb";
      inherit version;
      nativeBuildInputs = [ pkgs.dpkg ];
      dontUnpack = true;
      buildPhase =
        let
          arch = if pkgs.stdenv.hostPlatform.isAarch64 then "arm64" else "amd64";
        in
        ''
                  mkdir -p pkg/DEBIAN pkg/usr/bin
                  cp ${binPkg}/bin/${binName} pkg/usr/bin/
                  if [ -d "${binPkg}/share/man" ]; then
                    mkdir -p pkg/usr/share/man/man1
                    for f in ${binPkg}/share/man/man1/*.1; do
                      cp "$f" pkg/usr/share/man/man1/
                      gzip -9 "pkg/usr/share/man/man1/$(basename "$f")"
                    done
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

  blit-deb = mkDeb {
    pname = "blit";
    binPkg = blit-release;
    description = "blit terminal multiplexer";
    extraInstall =
      let
        systemdDir = ../systemd;
      in
      ''
        # No shared lib deps to bundle — all statically linked.
        mkdir -p pkg/lib/systemd/system
        cp "${systemdDir}/blit-server@.socket" "pkg/lib/systemd/system/blit-server@.socket"
        cp "${systemdDir}/blit-server@.service" "pkg/lib/systemd/system/blit-server@.service"
        cp "${systemdDir}/blit-share@.service" "pkg/lib/systemd/system/blit-share@.service"
        mkdir -p pkg/lib/systemd/user
        cp "${systemdDir}/blit-server.socket" "pkg/lib/systemd/user/blit-server.socket"
        cp "${systemdDir}/blit-server.service" "pkg/lib/systemd/user/blit-server.service"
        cp "${systemdDir}/blit.socket" "pkg/lib/systemd/user/blit.socket"
        cp "${systemdDir}/blit.service" "pkg/lib/systemd/user/blit.service"
      '';
  };

  publish-crates = pkgs.writeShellApplication {
    name = "blit-publish-crates";
    runtimeInputs = [
      rustToolchain
      pkgs.curl
      pkgs.jq
    ];
    text = ''
      usage() {
        echo "Usage: blit-publish-crates [--plan]"
      }

      plan_only=false
      case $# in
        0) ;;
        1)
          if [ "$1" != "--plan" ]; then
            usage >&2
            exit 2
          fi
          plan_only=true
          ;;
        *)
          usage >&2
          exit 2
          ;;
      esac

      metadata=$(cargo metadata --no-deps --format-version 1)

      mapfile -t crates < <(
        jq -r '
          .packages[]
          | select(.publish == null or (.publish | index("crates-io")))
          | .name
        ' <<<"$metadata"
      )
      if [ "''${#crates[@]}" -eq 0 ]; then
        echo "FATAL: workspace has no crates publishable to crates.io" >&2
        exit 1
      fi

      declare -A workspace_crates=()
      while IFS= read -r crate; do
        workspace_crates["$crate"]=1
      done < <(jq -r '.packages[].name' <<<"$metadata")

      declare -A publishable_crates=()
      for crate in "''${crates[@]}"; do
        publishable_crates["$crate"]=1
      done

      dependencies() {
        jq -r --arg crate "$1" '
          .packages[]
          | select(.name == $crate)
          | .dependencies[]
          | select(.kind != "dev" and .path != null)
          | .name
        ' <<<"$metadata"
      }

      for crate in "''${crates[@]}"; do
        while IFS= read -r dependency; do
          if [ -n "''${workspace_crates[$dependency]:-}" ] \
            && [ -z "''${publishable_crates[$dependency]:-}" ]; then
            echo "FATAL: publishable crate $crate depends on non-publishable workspace crate $dependency" >&2
            exit 1
          fi
        done < <(dependencies "$crate")
      done

      declare -A planned=()
      layers=()
      planned_count=0
      while [ "$planned_count" -lt "''${#crates[@]}" ]; do
        layer=()
        for crate in "''${crates[@]}"; do
          [ -n "''${planned[$crate]:-}" ] && continue

          ready=true
          while IFS= read -r dependency; do
            if [ -n "''${publishable_crates[$dependency]:-}" ] \
              && [ -z "''${planned[$dependency]:-}" ]; then
              ready=false
              break
            fi
          done < <(dependencies "$crate")

          if $ready; then
            layer+=("$crate")
          fi
        done

        if [ "''${#layer[@]}" -eq 0 ]; then
          echo "FATAL: workspace crate dependency graph contains a cycle" >&2
          exit 1
        fi

        layers+=("''${layer[*]}")
        for crate in "''${layer[@]}"; do
          planned["$crate"]=1
          planned_count=$((planned_count + 1))
        done
      done

      layer_number=0
      for layer in "''${layers[@]}"; do
        layer_number=$((layer_number + 1))
        echo "layer $layer_number: $layer"
      done

      $plan_only && exit 0

      if [ -z "''${CARGO_REGISTRY_TOKEN:-}" ] \
        && [ -n "''${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
        echo "=== Exchanging OIDC token for crates.io publish token ==="
        oidc_response=$(curl -sS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
          "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=crates.io")
        oidc=$(echo "$oidc_response" | jq -r '.value // empty')
        if [ -z "''${oidc:-}" ]; then
          echo "FATAL: failed to get OIDC token from GitHub"
          echo "Response: $oidc_response"
          exit 1
        fi

        token_response=$(curl -sS -X POST https://crates.io/api/v1/trusted_publishing/tokens \
          -H "Content-Type: application/json" \
          -d "{\"jwt\": \"$oidc\"}")
        token=$(echo "$token_response" | jq -r '.token // empty')
        if [ -z "''${token:-}" ]; then
          echo "FATAL: failed to exchange OIDC token for crates.io publish token"
          echo "Response: $token_response"
          exit 1
        fi
        export CARGO_REGISTRY_TOKEN="$token"
      fi

      [ -n "''${CARGO_REGISTRY_TOKEN:-}" ] || { echo "FATAL: no CARGO_REGISTRY_TOKEN and not in GitHub Actions"; exit 1; }

      VERSION=$(jq -r '
        [
          .packages[]
          | select(.publish == null or (.publish | index("crates-io")))
          | .version
        ]
        | unique
        | if length == 1 then .[0] else error("publishable workspace versions differ") end
      ' <<<"$metadata")

      is_published() {
        local code
        code=$(curl -s -o /dev/null -w '%{http_code}' \
          -A 'blit-release/1 (https://github.com/indent-com/blit)' \
          "https://crates.io/api/v1/crates/$1/$VERSION")
        [ "$code" = "200" ]
      }

      publish() {
        if is_published "$1"; then
          echo "--- $1@$VERSION already published, skipping ---"
          return 0
        fi
        echo "--- publishing $1 ---"
        cargo publish -p "$1" --no-verify
      }

      # Wait until every crate in a layer is indexed on crates.io before
      # proceeding to the next layer.  cargo publish returns before the
      # registry finishes indexing, so without this the next layer would
      # fail with "no matching package" errors.
      wait_for_layer() {
        for crate in "$@"; do
          local attempts=0
          while ! is_published "$crate"; do
            attempts=$((attempts + 1))
            if [ "$attempts" -ge 60 ]; then
              echo "ERROR: $crate@$VERSION not indexed after 5 minutes, giving up"
              exit 1
            fi
            echo "--- waiting for $crate@$VERSION to be indexed (attempt $attempts/60) ---"
            sleep 5
          done
          echo "--- $crate@$VERSION is available ---"
        done
      }

      for layer in "''${layers[@]}"; do
        read -r -a layer_crates <<<"$layer"
        for crate in "''${layer_crates[@]}"; do
          publish "$crate"
        done
        wait_for_layer "''${layer_crates[@]}"
      done
    '';
  };

  deploy-website = pkgs.writeShellApplication {
    name = "deploy-website";
    runtimeInputs = [
      pkgs.nodejs
      pkgs.pnpm
    ];
    text = ''
            tmp=$(mktemp -d)
            trap 'chmod -R u+w "$tmp" 2>/dev/null || true; rm -rf "$tmp"' EXIT

            mkdir -p "$tmp/.vercel/output/static"
            cp -r ${websiteDist}/* "$tmp/.vercel/output/static/"
            chmod -R u+w "$tmp"
            cat > "$tmp/.vercel/output/config.json" <<'JSON'
      {"version":3,"routes":[{"handle":"filesystem"},{"src":"/(.*)", "dest":"/index.html"}]}
      JSON

            if [ -n "''${VERCEL_ORG_ID:-}" ] && [ -n "''${VERCEL_PROJECT_ID:-}" ]; then
              cat > "$tmp/.vercel/project.json" <<PROJ
      {"orgId":"$VERCEL_ORG_ID","projectId":"$VERCEL_PROJECT_ID"}
      PROJ
            fi

            cd "$tmp"
            token_args=()
            if [ -n "''${VERCEL_TOKEN:-}" ]; then
              token_args+=(--token "$VERCEL_TOKEN")
            fi
            pnpm dlx vercel deploy --prebuilt "''${token_args[@]}" "$@"
    '';
  };

  fmt = pkgs.writeShellApplication {
    name = "blit-fmt";
    runtimeInputs = [
      rustToolchain
      pkgs.prettier
    ];
    text = ''
      check=false
      for arg in "$@"; do
        case "$arg" in
          --check) check=true ;;
        esac
      done

      # extensions/ is a cargo workspace of its own -- it only ever builds for
      # wasm -- so every root-workspace command has to be pointed at it too.
      #
      # From that directory, not with --manifest-path: extensions/Cargo.toml is
      # a virtual manifest, and cargo-fmt pointed at one has no "current crate"
      # to format -- it fails with "Failed to find targets" and prints its
      # usage, which reads like a broken invocation rather than an unformatted
      # tree. `--all` would answer that, but from here it also drags in path
      # dependencies outside the workspace (vendor/), so it formats files this
      # command has no business touching.
      if [ "$check" = true ]; then
        echo "=== cargo fmt --check ==="
        cargo fmt -- --check
        (cd extensions && cargo fmt -- --check)
        echo ""
        echo "=== prettier --check ==="
        prettier --check .
      else
        echo "=== cargo fmt ==="
        cargo fmt
        (cd extensions && cargo fmt)
        echo ""
        echo "=== prettier --write ==="
        prettier --write .
      fi
    '';
  };

  clippy = pkgs.writeShellApplication {
    name = "blit-clippy";
    runtimeInputs = [
      rustToolchain
      pkgs.pkg-config
      pkgs.libopus
    ]
    # x264-sys builds in the Linux-only feature-combo passes below: bindgen
    # dlopens nix's libclang, which requires the build scripts themselves to
    # be linked with nix's cc/glibc — a CI runner's system cc links them
    # against an older glibc that cannot load it.
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.x264
      pkgs.stdenv.cc
    ];
    text = ''
      export PKG_CONFIG_PATH="${pkgs.libopus.dev}/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
      export LIBRARY_PATH="${pkgs.libopus}/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}"

      # Clippy only needs these include_bytes! inputs to exist; building the
      # production UI here makes every lint run realize the browser WASM and
      # pnpm closures before Clippy can start. Preserve real development
      # assets, and remove any compile-only placeholders when lint exits.
      echo "=== Setting up UI dist inputs ==="
      mkdir -p js/ui/dist
      placeholder_assets=()
      cleanup_ui_dist() {
        if (( ''${#placeholder_assets[@]} )); then
          rm -f "''${placeholder_assets[@]}"
        fi
      }
      trap cleanup_ui_dist EXIT
      for asset in js/ui/dist/index.html.br js/ui/dist/sw.js.br \
        js/ui/dist/mux-worker.js.br js/ui/dist/buffer-recycler-worker.js.br; do
        if [ ! -e "$asset" ]; then
          : > "$asset"
          placeholder_assets+=("$asset")
        fi
      done

      echo "=== Clippy ==="
      cargo clippy --workspace -- -D warnings

      # The extensions workspace is excluded from the root one, so it needs its
      # own pass -- and only wasm32 is a meaningful target for it.
      echo "=== Clippy: extensions (wasm32) ==="
      cargo clippy --manifest-path extensions/Cargo.toml \
        --target wasm32-unknown-unknown --release -- -D warnings
    ''
    + pkgs.lib.optionalString pkgs.stdenv.isLinux ''
      # The software H.264 encoders are cargo features (default openh264,
      # x264 as the GPL opt-in, none = AV1-only software fallback) — keep
      # every combination compiling.  x264-sys needs pkg-config + bindgen.
      export PKG_CONFIG_PATH="${pkgs.x264.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
      export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
      export BINDGEN_EXTRA_CLANG_ARGS="-isystem ${pkgs.lib.getDev pkgs.stdenv.cc.libc}/include"
      cargo clippy -p blit-server --all-targets --all-features -- -D warnings
      cargo clippy -p blit-server --all-targets --no-default-features -- -D warnings
    '';
  };
  coverage = pkgs.writeShellApplication {
    name = "blit-coverage";
    runtimeInputs = [
      rustToolchain
      pkgs.cargo-llvm-cov
      pkgs.python3
      pkgs.pkg-config
      pkgs.libopus
    ];
    text = ''
      export PKG_CONFIG_PATH="${pkgs.libopus.dev}/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
      export LIBRARY_PATH="${pkgs.libopus}/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}"

      echo "=== Setting up UI dist ==="
      mkdir -p js/ui/dist
      for asset in index.html sw.js mux-worker.js buffer-recycler-worker.js; do
        rm -f "js/ui/dist/$asset" "js/ui/dist/$asset.br"
        cp ${webAppDist}/"$asset" ${webAppDist}/"$asset.br" js/ui/dist/
      done

      outdir="''${1:-coverage-report}"

      echo "=== Running tests with coverage ==="
      cargo llvm-cov --no-report --workspace

      echo ""
      echo "=== Coverage summary ==="
      cargo llvm-cov report --json > coverage.json
      python3 ${../bin/format-coverage.py}

      echo ""
      echo "=== Generating HTML report ==="
      cargo llvm-cov report --html --output-dir "$outdir"
      echo "HTML report written to $outdir/html/index.html"
    '';
  };

in
{
  inherit
    browser-publish
    js-publish
    publish-npm-packages
    publish-crates
    deploy-website
    ;
  inherit
    blit-deb
    ;
  inherit fmt clippy coverage;

  build-debs = pkgs.writeShellApplication {
    name = "blit-build-debs";
    text = ''
      outdir="''${1:-dist/debs}"
      mkdir -p "$outdir"
      cp ${blit-deb}/*.deb "$outdir"/
      ls -lh "$outdir"
    '';
  };

  # Build every publishable Wasm extension and describe the result.
  #
  # The manifest is the point: an extension is named by its BLAKE3 digest, so a
  # published URL is only pinnable (`ext run <url>#<digest>`) if the digest is
  # published with it.
  extensions = pkgs.writeShellApplication {
    name = "blit-extensions";
    runtimeInputs = [
      rustToolchain
      pkgs.binaryen
      pkgs.b3sum
      pkgs.brotli
      pkgs.jq
    ];
    text = ''
      cd extensions

      target=wasm32-unknown-unknown
      dist="''${1:-dist}"
      mkdir -p "$dist"

      # The digest is the module's identity, so it must not depend on where
      # the tree happens to sit: rustc bakes absolute paths into panic
      # locations, and a checkout at a different path hashes differently for
      # no reason a reader could ever see.
      cargo_home="''${CARGO_HOME:-$HOME/.cargo}"
      repo_root=$(cd .. && pwd)
      export RUSTFLAGS="--remap-path-prefix=$repo_root=/blit --remap-path-prefix=$cargo_home=/cargo''${RUSTFLAGS:+ $RUSTFLAGS}"

      cargo build --release --target "$target"

      metadata=$(cargo metadata --no-deps --format-version 1)
      version=$(jq -r '.packages[0].version' <<<"$metadata")

      # What each module is for, keyed by the name it is published under.
      # `package.description` is the one place that sentence already lives, and
      # the browser has nothing else to go on: the registry lists names and
      # digests, so without this an installable extension is an opaque word.
      # Keyed by the *bin* target's name because that is what the object is
      # called -- the crate is `blit-ext-systemd`, the module is `systemd.wasm`.
      descriptions=$(jq -c '
        [ .packages[]
          | . as $package
          | .targets[]
          | select(.kind | index("bin"))
          | { key: .name, value: ($package.description // "") }
        ] | from_entries' <<<"$metadata")
      entries=()

      for wasm in "target/$target/release"/*.wasm; do
        name=$(basename "$wasm" .wasm)
        out="$dist/$name.wasm"
        # -Oz over a module that is downloaded once and then cached by digest:
        # the only cost that matters here is bytes on the wire. -all because
        # rustc emits post-MVP features binaryen will not validate without it.
        #
        # Reference types are disabled back off again because -all also lets
        # binaryen *introduce* them, and the server's Wasmi validator rejects a
        # module that uses them: "function references required for index
        # reference types". That failure depends on what the optimizer finds to
        # do, so it appears when an extension grows an indirect call rather than
        # when the flags change — the module runs fine before wasm-opt and is
        # refused after it. The published artifact has to satisfy the loader,
        # not binaryen.
        #
        # Skipped when the input has not moved: cargo is already incremental,
        # and -Oz plus brotli -q 11 is the slow half of this script.
        if [ ! -s "$out" ] || [ "$wasm" -nt "$out" ]; then
          wasm-opt -Oz -all --disable-reference-types --disable-gc \
            --strip-debug --strip-producers "$wasm" -o "$out"
          brotli -f -q 11 -c "$out" >"$out.br"
        fi
        digest=$(b3sum --no-names "$out")
        bytes=$(wc -c <"$out")
        compressed=$(wc -c <"$out.br")
        description=$(jq -r --arg name "$name" '.[$name] // ""' <<<"$descriptions")
        entries+=("$(jq -n \
          --arg name "$name" \
          --arg description "$description" \
          --arg file "$name.wasm" \
          --arg digest "$digest" \
          --argjson bytes "$bytes" \
          --argjson compressed "$compressed" \
          '{name: $name, description: $description, file: $file, blake3: $digest, bytes: $bytes, brotli_bytes: $compressed}')")
        printf '%-12s %8s bytes  %8s brotli  %s\n' "$name" "$bytes" "$compressed" "$digest"
      done

      printf '%s\n' "''${entries[@]}" | jq -s \
        --arg version "$version" \
        '{version: $version, extensions: .}' >"$dist/manifest.json"

      echo ""
      echo "wrote $PWD/$dist/manifest.json"
    '';
  };

  build-tarballs = pkgs.writeShellApplication {
    name = "blit-build-tarballs";
    runtimeInputs = [ pkgs.gnutar ];
    text =
      let
        os = if pkgs.stdenv.isDarwin then "darwin" else "linux";
        arch = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
      in
      ''
        outdir="''${1:-dist/tarballs}"
        mkdir -p "$outdir"
      ''
      + (
        if pkgs.stdenv.isLinux then
          ''
            # glibc tarball: single binary (all deps statically linked, only glibc dynamic)
            tar --mode='u+w' -czf "$outdir/blit_${version}_${os}_${arch}.tar.gz" -C "${blit-release}" bin
            # musl tarball: single binary (needs system musl libc)
            tar --mode='u+w' -czf "$outdir/blit_${version}_${os}-musl_${arch}.tar.gz" -C "${blit-release-musl}" bin
            # GPL flavors: x264 software H.264 encoder instead of openh264
            # (opt-in via `curl install.blit.sh | BLIT_GPL=1 sh`)
            tar --mode='u+w' -czf "$outdir/blit-gpl_${version}_${os}_${arch}.tar.gz" -C "${blit-release-gnu-gpl}" bin
            tar --mode='u+w' -czf "$outdir/blit-gpl_${version}_${os}-musl_${arch}.tar.gz" -C "${blit-release-musl-gpl}" bin
          ''
        else
          ''
            # macOS: single binary
            tar --mode='u+w' -czf "$outdir/blit_${version}_${os}_${arch}.tar.gz" -C "${blit-release}" bin
          ''
      )
      + ''
        ls -lh "$outdir"
      '';
  };

  e2e = pkgs.writeShellApplication {
    name = "blit-e2e";
    runtimeInputs = [
      pkgs.nodejs
      # Use Nix's Playwright CLI so the JS package and browser bundle
      # come from the same nixpkgs revision. Do not install/run the npm
      # Playwright package here; its browser revision can drift from Nix.
      pkgs.playwright-test
    ];
    text = ''
      echo "=== Setting up binaries ==="
      mkdir -p target/debug
      ln -sf "${blit}/bin/blit" target/debug/blit

      echo "=== Running Playwright ==="
      (cd e2e && playwright test)
    '';
  };

  lint = pkgs.writeShellApplication {
    name = "blit-lint";
    runtimeInputs = [
      rustToolchain
      pkgs.pkg-config
      pkgs.libopus
    ];
    text = ''
      check=false
      for arg in "$@"; do
        case "$arg" in
          --check) check=true ;;
        esac
      done

      if [ "$check" = true ]; then
        ${fmt}/bin/blit-fmt --check
      else
        ${fmt}/bin/blit-fmt
      fi
      echo ""
      ${clippy}/bin/blit-clippy
    '';
  };

  deploy-hub = pkgs.writeShellApplication {
    name = "deploy-hub";
    runtimeInputs = [
      pkgs.flyctl
      pkgs.git
    ];
    text = ''
      root=$(git rev-parse --show-toplevel)
      flyctl deploy "$root/js/hub" "$@"
    '';
  };

  deploy-upsidedown = pkgs.writeShellApplication {
    name = "deploy-upsidedown";
    runtimeInputs = [
      pkgs.flyctl
      pkgs.git
    ];
    text = ''
      root=$(git rev-parse --show-toplevel)
      cd "$root"
      # Build context is the repo root (the Cargo workspace); config and
      # Dockerfile live under upsidedown/ and are passed explicitly. The
      # repo-root .dockerignore keeps target/ etc. out of the context.
      flyctl deploy . \
        --config upsidedown/fly.toml \
        --dockerfile upsidedown/Dockerfile "$@"
    '';
  };

  setup-hub = pkgs.writeShellApplication {
    name = "setup-hub";
    runtimeInputs = [
      pkgs.flyctl
      pkgs.git
    ];
    text = ''
      root=$(git rev-parse --show-toplevel)
      APP="blit-hub"
      ORG="''${FLY_ORG:-personal}"

      echo "=== Creating Fly app: $APP ==="
      flyctl apps create "$APP" --machines --org "$ORG" 2>/dev/null || echo "App $APP already exists, continuing..."

      if ! flyctl secrets list -a "$APP" 2>/dev/null | grep -q REDIS_URL; then
        if [ -z "''${REDIS_URL:-}" ]; then
          echo ""
          echo "ERROR: REDIS_URL is required. Provision Redis and pass the URL:"
          echo ""
          echo "  flyctl redis create --org $ORG"
          echo "  REDIS_URL=redis://... $0"
          exit 1
        fi
        echo ""
        echo "=== Setting REDIS_URL ==="
        flyctl secrets set REDIS_URL="$REDIS_URL" -a "$APP" --stage
      else
        echo ""
        echo "REDIS_URL already set, skipping."
      fi

      if [ -n "''${CF_TURN_TOKEN_ID:-}" ] && [ -n "''${CF_TURN_API_TOKEN:-}" ]; then
        echo ""
        echo "=== Setting Cloudflare TURN credentials ==="
        flyctl secrets set CF_TURN_TOKEN_ID="$CF_TURN_TOKEN_ID" CF_TURN_API_TOKEN="$CF_TURN_API_TOKEN" -a "$APP" --stage
      fi

      echo ""
      echo "=== Deploying ==="
      flyctl deploy "$root/js/hub" "$@"

      echo ""
      echo "=== Done ==="
      echo "App URL: https://$APP.fly.dev"
      echo ""
      echo "To enable CD from GitHub Actions, add a deploy token:"
      echo "  flyctl tokens create deploy -a $APP"
      echo "  gh secret set FLY_API_TOKEN --repo <owner>/<repo>"
    '';
  };

  tests = pkgs.writeShellApplication {
    name = "blit-tests";
    runtimeInputs = [
      rustToolchain
      pkgs.nodejs
      pkgs.pnpm
      pkgs.wasm-pack
      pkgs.wasm-bindgen-cli
      pkgs.python3
      pkgs.bun
      # The hub's tests drive a real redis: the outage they pin (registration
      # awaiting a dead socket) is invisible to anything that stubs it out.
      pkgs.valkey
      pkgs.pkg-config
      pkgs.libopus
    ]
    # x264-sys builds in the Linux-only feature-combo passes below: bindgen
    # dlopens nix's libclang, which requires the build scripts themselves to
    # be linked with nix's cc/glibc — a CI runner's system cc links them
    # against an older glibc that cannot load it.
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.x264
      pkgs.stdenv.cc
    ];
    text = ''
      export PKG_CONFIG_PATH="${pkgs.libopus.dev}/lib/pkgconfig''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
      export LIBRARY_PATH="${pkgs.libopus}/lib''${LIBRARY_PATH:+:$LIBRARY_PATH}"

      # Rust tests exercise the UI routes, so their include_bytes! inputs must
      # be valid Brotli streams with representative content. Building the
      # production UI here would realize the browser WASM and pnpm closures
      # before Cargo can start. Preserve real development assets and remove
      # only the small test fixtures created here.
      echo "=== Setting up UI dist inputs ==="
      mkdir -p js/ui/dist
      placeholder_assets=()
      cleanup_ui_dist() {
        if (( ''${#placeholder_assets[@]} )); then
          rm -f "''${placeholder_assets[@]}"
        fi
      }
      trap cleanup_ui_dist EXIT
      for asset in js/ui/dist/index.html.br js/ui/dist/sw.js.br \
        js/ui/dist/mux-worker.js.br js/ui/dist/buffer-recycler-worker.js.br; do
        if [ ! -e "$asset" ]; then
          case "$asset" in
            */index.html.br)
              fixture='<!doctype html><html><head><meta charset="utf-8"><title>blit test fixture</title></head><body><main id="root">blit</main></body></html>'
              ;;
            */sw.js.br)
              fixture='/* blit test fixture */ self.addEventListener("fetch", () => {});'
              ;;
            *-worker.js.br)
              fixture='/* blit test fixture */ self.addEventListener("message", () => {});'
              ;;
          esac
          node -e '
            const fs = require("node:fs");
            const zlib = require("node:zlib");
            fs.writeFileSync(process.argv[1], zlib.brotliCompressSync(Buffer.from(process.argv[2])));
          ' "$asset" "$fixture"
          placeholder_assets+=("$asset")
        fi
      done

      echo "=== Rust tests ==="
      cargo test --workspace
      echo ""

      # extensions/ is a workspace of its own, so the root run does not reach
      # it. An extension's pure logic — parsing, backoff — is host-testable and
      # is where its bugs live, so run it for the host rather than only linting
      # it for wasm.
      echo "=== Rust tests: extensions ==="
      cargo test --manifest-path extensions/Cargo.toml
      echo ""
    ''
    + pkgs.lib.optionalString pkgs.stdenv.isLinux ''
      echo "=== Rust tests: blit-server with both H.264 encoder features ==="
      export PKG_CONFIG_PATH="${pkgs.x264.dev}/lib/pkgconfig:$PKG_CONFIG_PATH"
      export LD_LIBRARY_PATH="${pkgs.x264.lib}/lib''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
      export LIBCLANG_PATH="${pkgs.llvmPackages.libclang.lib}/lib"
      export BINDGEN_EXTRA_CLANG_ARGS="-isystem ${pkgs.lib.getDev pkgs.stdenv.cc.libc}/include"
      cargo test -p blit-server --all-features
      echo ""
    ''
    + ''

      # JS tests need the browser module's generated declarations, but not an
      # optimized publish artifact. A local dev build is substantially faster
      # and reuses Cargo's target directory on subsequent runs.
      echo "=== Setting up browser WASM package ==="
      (cd crates/browser && wasm-pack build --target web --dev --out-dir pkg)
      node -e '
        const fs = require("fs");
        const path = "crates/browser/pkg/package.json";
        const pkg = JSON.parse(fs.readFileSync(path, "utf8"));
        pkg.name = "@blit-sh/browser";
        fs.writeFileSync(path, JSON.stringify(pkg));
      '

      echo "=== JS typecheck ==="
      (cd js && { pnpm install --frozen-lockfile 2>/dev/null || pnpm install; } && pnpm run typecheck)
      echo ""
      echo "=== JS workspace tests ==="
      (cd js && pnpm --parallel \
        --filter @blit-sh/core \
        --filter @blit-sh/react \
        --filter @blit-sh/solid \
        --filter @blit-sh/ui \
        run test)

      echo ""
      echo "=== Hub tests (real redis) ==="
      (cd js/hub && bun install --frozen-lockfile && bun run typecheck && bun test)

      # Reuse the dependency graph Cargo just compiled instead of making Nix
      # build a separate optimized blit package before this runner can start.
      echo ""
      echo "=== Building fd-channel test server ==="
      cargo build -p blit-cli --bin blit
      cargo_target_dir="''${CARGO_TARGET_DIR:-target}"
      if [[ "$cargo_target_dir" != /* ]]; then
        cargo_target_dir="$PWD/$cargo_target_dir"
      fi
      export BLIT_SERVER="$cargo_target_dir/debug/blit"
      echo ""
      echo "=== Python fd-channel test ==="
      python3 examples/fd-channel-python.py
      echo ""
      echo "=== Bun fd-channel test ==="
      bun run examples/fd-channel-bun.ts
    '';
  };
}
