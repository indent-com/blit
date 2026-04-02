{ pkgs, version, browserWasm, blit-server, blit-gateway
, blit-server-static, blit-cli-static, blit-gateway-static
, blit-webrtc-forwarder-static
, manPages, webAppDist, websiteDist, rustToolchain
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
    runtimeInputs = [ pkgs.nodejs pkgs.pnpm ];
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
    runtimeInputs = [ pkgs.nodejs pkgs.pnpm ];
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
      cp "${systemdDir}/blit-server@.socket" "pkg/lib/systemd/system/blit-server@.socket"
      cp "${systemdDir}/blit-server@.service" "pkg/lib/systemd/system/blit-server@.service"
      mkdir -p pkg/lib/systemd/user
      cp "${systemdDir}/blit-server.socket" "pkg/lib/systemd/user/blit-server.socket"
      cp "${systemdDir}/blit-server.service" "pkg/lib/systemd/user/blit-server.service"
    '';
  };

  blit-cli-deb = mkDeb {
    pname = "blit";
    binPkg = blit-cli-static;
    description = "blit terminal client";
    extraInstall = let
      systemdDir = ../systemd;
    in ''
      mkdir -p pkg/lib/systemd/user
      cp "${systemdDir}/blit.socket" "pkg/lib/systemd/user/blit.socket"
      cp "${systemdDir}/blit.service" "pkg/lib/systemd/user/blit.service"
    '';
  };

  blit-gateway-deb = mkDeb {
    pname = "blit-gateway";
    binPkg = blit-gateway-static;
    description = "blit WebSocket gateway";
  };

  blit-webrtc-forwarder-deb = mkDeb {
    pname = "blit-webrtc-forwarder";
    binPkg = blit-webrtc-forwarder-static;
    description = "blit WebRTC forwarder";
    extraInstall = let
      systemdDir = ../systemd;
    in ''
      mkdir -p pkg/lib/systemd/system
      cp "${systemdDir}/blit-webrtc-forwarder@.service" "pkg/lib/systemd/system/blit-webrtc-forwarder@.service"
    '';
  };

  publish-crates = pkgs.writeShellApplication {
    name = "blit-publish-crates";
    runtimeInputs = [ rustToolchain pkgs.curl pkgs.jq ];
    text = ''
      if [ -n "''${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
        echo "=== Exchanging OIDC token for crates.io publish token ==="
        oidc_response=$(curl -sS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
          "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=https://crates.io")
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

      publish() {
        echo "--- publishing $1 ---"
        cargo publish -p "$1" --no-verify
      }

      publish blit-fonts
      publish blit-remote
      echo "waiting for crates.io to index..."
      sleep 30

      publish blit-webserver
      publish blit-alacritty
      echo "waiting for crates.io to index..."
      sleep 30

      publish blit-server
      publish blit-webrtc-forwarder
      publish blit-cli
      publish blit-gateway
    '';
  };

  deploy-website = pkgs.writeShellApplication {
    name = "deploy-website";
    runtimeInputs = [ pkgs.nodejs pkgs.pnpm ];
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
    runtimeInputs = [ rustToolchain pkgs.prettier ];
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
      echo "=== Setting up web-app dist ==="
      mkdir -p js/web-app/dist
      cp ${webAppDist}/index.html js/web-app/dist/

      echo "=== Clippy ==="
      cargo clippy --workspace -- -D warnings
    '';
  };
in {
  inherit browser-publish js-publish publish-npm-packages publish-crates deploy-website;
  inherit blit-server-deb blit-cli-deb blit-gateway-deb blit-webrtc-forwarder-deb;
  inherit fmt clippy;

  build-debs = pkgs.writeShellApplication {
    name = "blit-build-debs";
    text = ''
      outdir="''${1:-dist/debs}"
      mkdir -p "$outdir"
      for pkg in ${blit-server-deb} ${blit-cli-deb} ${blit-gateway-deb} ${blit-webrtc-forwarder-deb}; do
        cp "$pkg"/*.deb "$outdir"/
      done
      echo ""
      ls -lh "$outdir"
    '';
  };

  build-tarballs = pkgs.writeShellApplication {
    name = "blit-build-tarballs";
    text = let
      os = if pkgs.stdenv.isDarwin then "darwin" else "linux";
      arch = if pkgs.stdenv.hostPlatform.isAarch64 then "aarch64" else "x86_64";
    in ''
      outdir="''${1:-dist/tarballs}"
      mkdir -p "$outdir"
      for pkg in ${blit-server-static} ${blit-cli-static} ${blit-gateway-static} ${blit-webrtc-forwarder-static}; do
        for bin in "$pkg"/bin/*; do
          name=$(basename "$bin")
          tar -czf "$outdir/''${name}_${version}_${os}_${arch}.tar.gz" -C "$pkg/bin" "$name"
        done
      done
      echo ""
      ls -lh "$outdir"
    '';
  };

  e2e = pkgs.writeShellApplication {
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
      (cd e2e && pnpm exec playwright test)
    '';
  };

  lint = pkgs.writeShellApplication {
    name = "blit-lint";
    text = ''
      ${fmt}/bin/blit-fmt --check
      echo ""
      ${clippy}/bin/blit-clippy
    '';
  };

  deploy-blit-hub = pkgs.writeShellApplication {
    name = "deploy-blit-hub";
    runtimeInputs = [ pkgs.flyctl pkgs.git ];
    text = ''
      root=$(git rev-parse --show-toplevel)
      flyctl deploy "$root/js/blit-hub" "$@"
    '';
  };

  setup-blit-hub = pkgs.writeShellApplication {
    name = "setup-blit-hub";
    runtimeInputs = [ pkgs.flyctl pkgs.git ];
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
      flyctl deploy "$root/js/blit-hub" "$@"

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
    runtimeInputs = [ rustToolchain pkgs.nodejs pkgs.pnpm pkgs.scdoc pkgs.python3 pkgs.bun ];
    text = ''
      echo "=== Setting up web-app dist ==="
      mkdir -p js/web-app/dist
      cp ${webAppDist}/index.html js/web-app/dist/

      echo "=== Manpage build ==="
      for f in man/*.scd; do
        scdoc < "$f" > /dev/null
      done

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

      export BLIT_SERVER="${blit-server}/bin/blit-server"
      echo ""
      echo "=== Python fd-channel test ==="
      python3 examples/fd-channel-python.py
      echo ""
      echo "=== Bun fd-channel test ==="
      bun run examples/fd-channel-bun.ts
    '';
  };
}
