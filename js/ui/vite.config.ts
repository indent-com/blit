import { defineConfig, type Plugin } from "vite";
import { lezer } from "@lezer/generator/rollup";
import solid from "vite-plugin-solid";
import { viteSingleFile } from "vite-plugin-singlefile";
import { readFileSync, writeFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";
import { brotliCompressSync, constants as zlibConstants } from "node:zlib";
import { request as httpRequest } from "node:http";
import { Socket } from "node:net";

const wasmPath = resolve(
  __dirname,
  "../../crates/browser/pkg/blit_browser_bg.wasm",
);
const snippetsDir = resolve(__dirname, "../../crates/browser/pkg/snippets");
const isDev =
  process.env.NODE_ENV !== "production" && !process.argv.includes("build");

/** Dev-only path of the service worker entry, mirroring SW_DEV_ENTRY in
 *  src/preview.ts. */
const SW_DEV_ENTRY = "/src/sw/index.ts";

export default defineConfig({
  base: "/",
  plugins: [
    solid(),
    // Compiles src/ide/nix/syntax.grammar to a Lezer parser at build time.
    // The grammar is vendored (and patched — see its header) because the
    // only published Nix grammar mis-parses formatted Nix.
    lezer(),
    // Only inline everything into a single HTML file for production builds.
    !isDev && viteSingleFile(),
    {
      name: "inline-wasm",
      resolveId(id) {
        if (id === "virtual:blit-wasm") return "\0virtual:blit-wasm";
      },
      load(id) {
        if (id !== "\0virtual:blit-wasm") return;
        if (isDev) {
          // In dev, use a URL import so Vite serves the file directly.
          return `export default "/@fs${wasmPath}";`;
        }
        const wasm = readFileSync(wasmPath);
        const b64 = wasm.toString("base64");
        return `
const b64 = ${JSON.stringify(b64)};
const bin = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
export default bin.buffer;
`;
      },
    },
    {
      name: "resolve-blit-snippets",
      resolveId(id, importer) {
        const match = id.match(/\.\/snippets\/blit-browser-[^/]+\/(.*)/);
        if (match && importer && existsSync(snippetsDir)) {
          const file = match[1];
          for (const dir of readdirSync(snippetsDir)) {
            const candidate = join(snippetsDir, dir, file);
            if (existsSync(candidate)) return candidate;
          }
        }
      },
    },
    // Dev: serve the preview service worker from source. The dev server has
    // no /sw.js route — its SPA fallback answers with index.html, and a worker
    // served as text/html is refused outright. Vite transforms the TS entry to
    // a real module; the header is what lets a script under /src claim the
    // whole origin as its scope.
    isDev && {
      name: "blit-sw-dev",
      configureServer(server) {
        server.middlewares.use((req, res, next) => {
          if (req.url && req.url.startsWith(SW_DEV_ENTRY)) {
            res.setHeader("Service-Worker-Allowed", "/");
          }
          next();
        });
      },
    },
    // Dev: proxy blit WS connections to the gateway.
    isDev && {
      name: "blit-dev-proxy",
      configureServer(server) {
        const proxySockets = new Set<import("node:stream").Duplex>();
        const gwHost =
          process.env.VITE_BLIT_GATEWAY ||
          `localhost:${process.env.BLIT_DEV_GW_PORT || "3266"}`;
        const [gwHostname, gwPort] = gwHost.includes(":")
          ? [
              gwHost.slice(0, gwHost.lastIndexOf(":")),
              gwHost.slice(gwHost.lastIndexOf(":") + 1),
            ]
          : [gwHost, "80"];

        function proxyWsToGateway(
          req: import("node:http").IncomingMessage,
          socket: import("node:stream").Duplex,
          head: Buffer,
          gwPath: string,
        ) {
          // This is a frame transport, not an HTTP bulk transfer. Do not let
          // either local proxy leg wait for a delayed ACK before forwarding a
          // completed WebSocket message.
          if (socket instanceof Socket) socket.setNoDelay(true);
          const proxyReq = httpRequest({
            hostname: gwHostname,
            port: parseInt(gwPort),
            path: gwPath,
            method: req.method,
            headers: req.headers,
          });
          proxyReq.on("upgrade", (_res, proxySocket, proxyHead) => {
            // Both legs are loopback sockets carrying latency-sensitive
            // protocol messages. Make the intent explicit rather than
            // inheriting a Node version's Nagle default.
            if ("setNoDelay" in socket) socket.setNoDelay(true);
            proxySocket.setNoDelay(true);

            const closeBoth = () => {
              proxySockets.delete(socket);
              proxySockets.delete(proxySocket);
              socket.destroy();
              proxySocket.destroy();
            };
            // pipe() does not consume stream errors. A client reload used to
            // turn a routine EPIPE into an uncaught exception that restarted
            // the whole Vite process, pausing every other connection too.
            socket.on("error", closeBoth);
            proxySocket.on("error", closeBoth);
            socket.on("close", closeBoth);
            proxySocket.on("close", closeBoth);
            proxySockets.add(socket);
            proxySockets.add(proxySocket);

            socket.write(
              "HTTP/1.1 101 Switching Protocols\r\n" +
                "Upgrade: websocket\r\n" +
                "Connection: Upgrade\r\n" +
                `Sec-WebSocket-Accept: ${_res.headers["sec-websocket-accept"]}\r\n` +
                (_res.headers["sec-websocket-protocol"]
                  ? `Sec-WebSocket-Protocol: ${_res.headers["sec-websocket-protocol"]}\r\n`
                  : "") +
                "\r\n",
            );
            if (proxyHead.length) socket.write(proxyHead);
            // Bytes following the browser's upgrade headers belong to the
            // WebSocket stream and must be forwarded to the gateway.
            if (head.length) proxySocket.write(head);
            proxySocket.pipe(socket);
            socket.pipe(proxySocket);
          });
          proxyReq.on("error", () => socket.destroy());
          proxyReq.end();
        }

        const onUpgrade = (
          req: import("node:http").IncomingMessage,
          socket: import("node:stream").Duplex,
          head: Buffer,
        ) => {
          const path = req.url || "/";

          // Let Vite handle its own WS connections (HMR, etc.).
          if (path.startsWith("/__") || path.startsWith("/@")) return;
          const url = new URL(path, "http://localhost");
          if (url.searchParams.has("token")) return;

          // Blit WebSocket connections (config sync, /d/… transport) → gateway.
          proxyWsToGateway(req, socket, head, path);
        };
        server.httpServer?.on("upgrade", onUpgrade);
        server.httpServer?.once("close", () => {
          server.httpServer?.off("upgrade", onUpgrade);
          for (const socket of proxySockets) socket.destroy();
          proxySockets.clear();
        });
      },
    },
    !isDev && {
      name: "brotli-html",
      closeBundle() {
        // The workers alongside index.html: each is served separately by the
        // gateway, which only ever sends brotli, so each needs its own `.br`.
        const assets = [
          "dist/index.html",
          "dist/mux-worker.js",
          "dist/buffer-recycler-worker.js",
        ];
        for (const asset of assets) {
          const path = resolve(__dirname, asset);
          if (!existsSync(path)) continue;
          const contents = readFileSync(path);
          const compressed = brotliCompressSync(contents, {
            params: {
              [zlibConstants.BROTLI_PARAM_QUALITY]:
                zlibConstants.BROTLI_MAX_QUALITY,
            },
          });
          writeFileSync(path + ".br", compressed);
        }
      },
    },
  ].filter(Boolean),
  resolve: {
    alias: {
      "@blit-sh/browser": resolve(
        __dirname,
        "../../crates/browser/pkg/blit_browser.js",
      ),
    },
    dedupe: ["solid-js"],
  },
  server: {
    port: parseInt(process.env.BLIT_DEV_UI_PORT || "3265"),
    host: "0.0.0.0",
    allowedHosts: true,
    fs: {
      // Allow serving the WASM file from outside the ui directory.
      allow: [resolve(__dirname, "../..")],
    },
    proxy: isDev
      ? (() => {
          const gw = `http://${process.env.VITE_BLIT_GATEWAY || `localhost:${process.env.BLIT_DEV_GW_PORT || "3266"}`}`;
          // The stack's extension registry (bin/ext-registry), served under
          // the page's own origin. It listens on loopback and nothing
          // publishes it, so a page reached over a tunnel or a reverse proxy
          // can only get at it through here — see defaultRegistry().
          const ext = `http://localhost:${process.env.BLIT_DEV_EXT_PORT || "3268"}`;
          return {
            "/fonts": { target: gw },
            "/font": { target: gw },
            "/ext": {
              target: ext,
              rewrite: (path: string) => path.replace(/^\/ext/, ""),
            },
          };
        })()
      : undefined,
  },
  build: {
    outDir: resolve(__dirname, "dist"),
    target: "es2020",
  },
  // Workers cannot be inlined into the single-file build, so they ship as
  // their own assets and the gateway has to serve each one by name. A
  // content-hashed name cannot be named by `include_bytes!`, and a worker the
  // gateway does not serve is a worker that 404s in production while working
  // perfectly in dev — so the names are pinned.
  worker: {
    format: "es",
    rollupOptions: {
      output: {
        entryFileNames: "[name].js",
        chunkFileNames: "[name].js",
      },
    },
  },
});
