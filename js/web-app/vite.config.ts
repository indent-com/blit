import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";

const wasmPath = resolve(
  __dirname,
  "../../crates/browser/pkg/blit_browser_bg.wasm",
);
const snippetsDir = resolve(__dirname, "../../crates/browser/pkg/snippets");
const isDev =
  process.env.NODE_ENV !== "production" && !process.argv.includes("build");

export default defineConfig({
  plugins: [
    react(),
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
  ].filter(Boolean),
  resolve: {
    alias: {
      "@blit-sh/browser": resolve(
        __dirname,
        "../../crates/browser/pkg/blit_browser.js",
      ),
    },
    dedupe: ["react", "react-dom"],
  },
  server: {
    port: 3265,
    fs: {
      // Allow serving the WASM file from outside the web-app directory.
      allow: [resolve(__dirname, "../..")],
    },
  },
  build: {
    outDir: resolve(__dirname, "dist"),
    target: "es2020",
  },
});
