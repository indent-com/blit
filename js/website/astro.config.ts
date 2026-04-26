import { defineConfig, fontProviders } from "astro/config";
import solidJs from "@astrojs/solid-js";
import tailwindcss from "@tailwindcss/vite";
import basicSsl from "@vitejs/plugin-basic-ssl";
import { readFileSync, existsSync, readdirSync } from "node:fs";
import { resolve, join } from "node:path";

const __dirname = import.meta.dirname;

const wasmPath = resolve(
  __dirname,
  "../../crates/browser/pkg/blit_browser_bg.wasm",
);
const snippetsDir = resolve(__dirname, "../../crates/browser/pkg/snippets");

export default defineConfig({
  integrations: [solidJs()],
  fonts: [
    {
      provider: fontProviders.fontsource(),
      name: "Fira Code",
      cssVariable: "--font-mono",
      weights: [400, 700],
      styles: ["normal"],
      fallbacks: ["ui-monospace", "monospace"],
    },
    {
      provider: fontProviders.fontsource(),
      name: "Inter",
      cssVariable: "--font-sans",
      weights: [400, 500, 600, 700],
      styles: ["normal"],
      fallbacks: ["ui-sans-serif", "system-ui", "sans-serif"],
    },
  ],
  vite: {
    plugins: [
      basicSsl(),
      tailwindcss(),
      {
        name: "inline-wasm",
        resolveId(id) {
          if (id === "virtual:blit-wasm") return "\0virtual:blit-wasm";
        },
        load: {
          order: "pre",
          handler(id) {
            if (id !== "\0virtual:blit-wasm") return;
            // In SSR environments, WASM can't be loaded — return null stub
            if (
              this.environment?.name === "server" ||
              this.environment?.name === "ssr"
            ) {
              return `export default null;`;
            }
            // Dev: serve WASM file directly via Vite's FS server
            if (
              this.environment?.mode === "dev" ||
              (!process.argv.includes("build") &&
                process.env.NODE_ENV !== "production")
            ) {
              return `export default "/@fs${wasmPath}";`;
            }
            // Prod client build: inline WASM as base64
            const wasmBytes = readFileSync(wasmPath);
            const b64 = wasmBytes.toString("base64");
            return `
const b64 = ${JSON.stringify(b64)};
const bin = Uint8Array.from(atob(b64), c => c.charCodeAt(0));
export default bin.buffer;
`;
          },
        },
      },
      {
        name: "resolve-blit-snippets",
        resolveId(id, importer) {
          const match = id.match(/\.\/(snippets)\/blit-browser-[^/]+\/(.*)/);
          if (match && importer && existsSync(snippetsDir)) {
            const file = match[2];
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
    },
    server: {
      fs: {
        allow: [resolve(__dirname, "../..")],
      },
    },
  },
});
