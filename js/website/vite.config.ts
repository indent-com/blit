import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";

const wasmPath = resolve(
  __dirname,
  "../../crates/browser/pkg/blit_browser_bg.wasm",
);
const snippetsDir = resolve(__dirname, "../../crates/browser/pkg/snippets");
const isDev =
  process.env.NODE_ENV !== "production" && !process.argv.includes("build");
const isSsr = process.argv.includes("--ssr");

export default defineConfig({
  plugins: [
    react(),
    {
      name: "inline-wasm",
      resolveId(id) {
        if (id === "virtual:blit-wasm") return "\0virtual:blit-wasm";
      },
      load(id) {
        if (id !== "\0virtual:blit-wasm") return;
        if (isSsr) {
          return `export default null;`;
        }
        if (isDev) {
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
  ],
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
    port: 5173,
    fs: {
      allow: [resolve(__dirname, "../..")],
    },
  },
  build: {
    outDir: resolve(__dirname, isSsr ? "dist-ssr" : "dist"),
    target: "es2020",
  },
});
