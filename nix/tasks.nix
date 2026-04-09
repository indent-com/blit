{
  pkgs,
  version,
  browserWasm,
  blit,
  blit-static,
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
    echo '{"name":"@blit-sh/browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > crates/browser/pkg/package.json
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

            cat > "$tmp/package.json" <<'PKGJSON'
      {
        "name": "@blit-sh/browser",
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
    binPkg = blit-static;
    description = "blit terminal multiplexer";
    extraInstall =
      let
        systemdDir = ../systemd;
      in
      ''
        mkdir -p pkg/lib/systemd/system
        cp "${systemdDir}/blit-server@.socket" "pkg/lib/systemd/system/blit-server@.socket"
        cp "${systemdDir}/blit-server@.service" "pkg/lib/systemd/system/blit-server@.service"
        cp "${systemdDir}/blit-webrtc-forwarder@.service" "pkg/lib/systemd/system/blit-webrtc-forwarder@.service"
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
      if [ -n "''${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
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

      VERSION=$(cargo metadata --no-deps --format-version 1 \
        | jq -r '[.packages[].version] | unique | if length == 1 then .[0] else error("workspace versions differ") end')

      is_published() {
        local code
        code=$(curl -s -o /dev/null -w '%{http_code}' "https://crates.io/api/v1/crates/$1/$VERSION")
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

      # Layer 1: leaf crates (no workspace deps)
      publish blit-fonts
      publish blit-remote
      publish blit-ssh
      publish blit-compositor
      wait_for_layer blit-fonts blit-remote blit-ssh blit-compositor

      # Layer 2: depend only on leaf crates
      publish blit-webserver
      publish blit-alacritty
      publish blit-webrtc-forwarder
      wait_for_layer blit-webserver blit-alacritty blit-webrtc-forwarder

      # Layer 3: depend on layer 1+2
      publish blit-server
      publish blit-proxy
      publish blit-gateway
      wait_for_layer blit-server blit-proxy blit-gateway

      # Layer 4: depends on nearly everything
      publish blit-cli
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
            trap 'rm -rf "$tmp"' EXIT

            mkdir -p "$tmp/.vercel/output/static"
            cp -r ${websiteDist}/* "$tmp/.vercel/output/static/"
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

      if [ "$check" = true ]; then
        echo "=== cargo fmt --check ==="
        cargo fmt -- --check
        echo ""
        echo "=== prettier --check ==="
        prettier --check .
      else
        echo "=== cargo fmt ==="
        cargo fmt
        echo ""
        echo "=== prettier --write ==="
        prettier --write .
      fi
    '';
  };

  clippy = pkgs.writeShellApplication {
    name = "blit-clippy";
    runtimeInputs = [ rustToolchain ];
    text = ''
      echo "=== Setting up UI dist ==="
      mkdir -p js/ui/dist
      cp ${webAppDist}/index.html ${webAppDist}/index.html.br js/ui/dist/

      echo "=== Clippy ==="
      cargo clippy --workspace -- -D warnings
    '';
  };
  coverage = pkgs.writeShellApplication {
    name = "blit-coverage";
    runtimeInputs = [
      rustToolchain
      pkgs.cargo-llvm-cov
      pkgs.python3
      pkgs.pkg-config
      pkgs.libxkbcommon
      pkgs.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.libgbm
    ];
    text = ''
      export PKG_CONFIG_PATH="${pkgs.libxkbcommon.dev}/lib/pkgconfig:${pkgs.pixman}/lib/pkgconfig${
        if pkgs.stdenv.isLinux then ":${pkgs.libgbm}/lib/pkgconfig" else ""
      }''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
      export LIBRARY_PATH="${pkgs.libxkbcommon}/lib:${pkgs.pixman}/lib${
        if pkgs.stdenv.isLinux then ":${pkgs.libgbm}/lib" else ""
      }''${LIBRARY_PATH:+:$LIBRARY_PATH}"

      echo "=== Setting up UI dist ==="
      mkdir -p js/ui/dist
      cp ${webAppDist}/index.html ${webAppDist}/index.html.br js/ui/dist/

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

  build-tarballs = pkgs.writeShellApplication {
    name = "blit-build-tarballs";
    text =
      let
        os = if pkgs.stdenv.isDarwin then "darwin" else "linux";
        arch = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
      in
      ''
        outdir="''${1:-dist/tarballs}"
        mkdir -p "$outdir"
        tar -czf "$outdir/blit_${version}_${os}_${arch}.tar.gz" -C "${blit-static}/bin" blit
        ls -lh "$outdir"
      '';
  };

  e2e = pkgs.writeShellApplication {
    name = "blit-e2e";
    runtimeInputs = [
      pkgs.nodejs
      pkgs.pnpm
    ];
    text = ''
      export PLAYWRIGHT_BROWSERS_PATH="${pkgs.playwright-driver.browsers}"
      export PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1

      echo "=== Setting up binaries ==="
      mkdir -p target/debug
      ln -sf "${blit}/bin/blit" target/debug/blit

      echo "=== Installing e2e deps ==="
      (cd e2e && if ! pnpm install --frozen-lockfile 2>/dev/null; then pnpm install; fi)

      echo "=== Running Playwright ==="
      (cd e2e && pnpm exec playwright test)
    '';
  };

  lint = pkgs.writeShellApplication {
    name = "blit-lint";
    runtimeInputs = [
      rustToolchain
      pkgs.pkg-config
      pkgs.libxkbcommon
      pkgs.pixman
    ];
    text = ''
      ${fmt}/bin/blit-fmt --check
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
      pkgs.python3
      pkgs.bun
      pkgs.pkg-config
      pkgs.libxkbcommon
      pkgs.pixman
    ]
    ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
      pkgs.libgbm
    ];
    text = ''
      export PKG_CONFIG_PATH="${pkgs.libxkbcommon.dev}/lib/pkgconfig:${pkgs.pixman}/lib/pkgconfig${
        if pkgs.stdenv.isLinux then ":${pkgs.libgbm}/lib/pkgconfig" else ""
      }''${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
      export LIBRARY_PATH="${pkgs.libxkbcommon}/lib:${pkgs.pixman}/lib${
        if pkgs.stdenv.isLinux then ":${pkgs.libgbm}/lib" else ""
      }''${LIBRARY_PATH:+:$LIBRARY_PATH}"

      echo "=== Setting up UI dist ==="
      mkdir -p js/ui/dist
      cp ${webAppDist}/index.html ${webAppDist}/index.html.br js/ui/dist/

      echo "=== Rust tests ==="
      cargo test --workspace
      echo ""

      echo "=== Setting up browser WASM package ==="
      ${setupBrowserPkg}

      echo "=== JS typecheck ==="
      (cd js && { pnpm install --frozen-lockfile 2>/dev/null || pnpm install; } && pnpm run typecheck)
      echo ""
      echo "=== JS workspace tests ==="
      (cd js && pnpm --filter @blit-sh/core run test && pnpm --filter @blit-sh/react run test && pnpm --filter @blit-sh/solid run test)

      export BLIT_SERVER="${blit}/bin/blit"
      echo ""
      echo "=== Python fd-channel test ==="
      python3 examples/fd-channel-python.py
      echo ""
      echo "=== Bun fd-channel test ==="
      bun run examples/fd-channel-bun.ts
    '';
  };
}
