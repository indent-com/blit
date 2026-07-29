import { defineConfig } from "vite";
import { brotliCompressSync, constants as zlibConstants } from "node:zlib";
import { readFileSync, writeFileSync } from "node:fs";
import { resolve } from "node:path";

/**
 * Second build, for the preview service worker (docs/design/net.md § Reserve
 * the prefix server-side).
 *
 * The app build inlines everything into one HTML file with
 * `vite-plugin-singlefile`, and a service worker cannot be inlined — it needs
 * its own URL and a JavaScript MIME type. So it gets its own config: no
 * single-file plugin, one entry, an IIFE bundle at a stable name the gateway
 * embeds beside `index.html.br`.
 */
export default defineConfig({
  build: {
    outDir: "dist",
    // The app build runs first and its output must survive this one.
    emptyOutDir: false,
    target: "es2022",
    rollupOptions: {
      input: resolve(__dirname, "src/sw/index.ts"),
      output: {
        // A service worker cannot be an ES module in every browser that
        // otherwise supports one, and this bundle has no reason to be: it is
        // self-contained.
        format: "iife",
        entryFileNames: "sw.js",
      },
    },
    minify: "esbuild",
  },
  plugins: [
    {
      name: "brotli-sw",
      closeBundle() {
        const path = resolve(__dirname, "dist/sw.js");
        const source = readFileSync(path);
        writeFileSync(
          path + ".br",
          brotliCompressSync(source, {
            params: {
              [zlibConstants.BROTLI_PARAM_QUALITY]: 11,
              [zlibConstants.BROTLI_PARAM_SIZE_HINT]: source.length,
            },
          }),
        );
      },
    },
  ],
});
