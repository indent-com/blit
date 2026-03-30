{ pkgs, version, browserWasm, blit-server, blit-gateway
, blit-server-static, blit-cli-static, blit-gateway-static
, manPages, webAppDist, rustToolchain
}:
let
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

      wasm="$tmp/blit-browser"
      mkdir -p "$wasm"
      cp ${browserWasm}/blit_browser.js ${browserWasm}/blit_browser.d.ts "$wasm"/
      cp ${browserWasm}/blit_browser_bg.wasm.d.ts "$wasm"/ 2>/dev/null || true
      echo '{"name":"blit-browser","version":"${version}","main":"blit_browser.js","types":"blit_browser.d.ts"}' > "$wasm/package.json"

      cp -a ${../react}/* "$tmp"/
      chmod -R u+w "$tmp"

      cd "$tmp"
      ${pkgs.nodejs}/bin/npm pkg set "devDependencies.blit-browser=file:$wasm"
      npm install
      npm run build

      echo "Package contents:"
      ls -lh dist/
      echo ""
      npm publish "$@"
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
  publish-npm-packages = pkgs.writeShellApplication {
    name = "blit-publish-npm-packages";
    runtimeInputs = [ pkgs.nodejs ];
    text = ''
      echo "=== Publishing blit-browser ==="
      ${browser-publish}/bin/browser-publish "$@"
      echo ""
      echo "=== Publishing blit-react ==="
      ${react-publish}/bin/react-publish "$@"
    '';
  };

  publish-crates = pkgs.writeShellApplication {
    name = "blit-publish-crates";
    runtimeInputs = [ rustToolchain pkgs.curl pkgs.jq ];
    text = ''
      if [ -n "''${ACTIONS_ID_TOKEN_REQUEST_TOKEN:-}" ]; then
        echo "=== Exchanging OIDC token for crates.io publish token ==="
        oidc=$(curl -sS -H "Authorization: bearer $ACTIONS_ID_TOKEN_REQUEST_TOKEN" \
          "$ACTIONS_ID_TOKEN_REQUEST_URL&audience=https://crates.io" | jq -r '.value')
        token=$(curl -sS -X POST https://crates.io/api/v1/trusted_publishing/tokens \
          -H "Authorization: Bearer $oidc" | jq -r '.token')
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
      publish blit-cli
      publish blit-gateway
    '';
  };
in {
  inherit browser-publish react-publish publish-npm-packages publish-crates;
  inherit blit-server-deb blit-cli-deb blit-gateway-deb;

  build-debs = pkgs.writeShellApplication {
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

  build-tarballs = pkgs.writeShellApplication {
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
      (cd e2e && npx playwright test)
    '';
  };

  lint = pkgs.writeShellApplication {
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

  tests = pkgs.writeShellApplication {
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
}
