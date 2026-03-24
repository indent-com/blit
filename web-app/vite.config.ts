import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { viteSingleFile } from "vite-plugin-singlefile";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";

const wasmPath = resolve(__dirname, "../browser/pkg/blit_browser_bg.wasm");
const snippetsDir = resolve(__dirname, "../browser/pkg/snippets");

export default defineConfig({
  plugins: [
    react(),
    viteSingleFile(),
    {
      name: "inline-wasm",
      resolveId(id) {
        if (id === "virtual:blit-wasm") return "\0virtual:blit-wasm";
      },
      load(id) {
        if (id === "\0virtual:blit-wasm") {
          const wasm = readFileSync(wasmPath);
          const b64 = wasm.toString("base64");
          return `
const b64 = ${JSON.stringify(b64)};
const bin = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
export default bin.buffer;
`;
        }
      },
    },
    {
      name: "resolve-blit-snippets",
      resolveId(id, importer) {
        // Resolve ./snippets/blit-browser-xxx/inline0.js imports from blit_browser.js
        const match = id.match(/\.\/snippets\/(blit-browser-[^/]+)\/(.*)/);
        if (match && importer) {
          // Try the exact path first
          const exact = join(snippetsDir, match[1], match[2]);
          if (existsSync(exact)) return exact;
          // Fall back to any blit-browser-* directory (hash changes per build)
          if (existsSync(snippetsDir)) {
            for (const dir of readdirSync(snippetsDir)) {
              const candidate = join(snippetsDir, dir, match[2]);
              if (existsSync(candidate)) return candidate;
            }
          }
        }
      },
    },
  ],
  resolve: {
    alias: {
      "blit-react": resolve(__dirname, "../react/src"),
      "blit-browser": resolve(__dirname, "../browser/pkg/blit_browser.js"),
    },
  },
  build: {
    outDir: resolve(__dirname, "dist"),
    target: "es2020",
  },
});
