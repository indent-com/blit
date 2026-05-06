import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import { resolve } from "path";

export default defineConfig({
  plugins: [solid()],
  build: {
    lib: {
      entry: {
        index: resolve(__dirname, "src/index.ts"),
        "hooks/index": resolve(__dirname, "src/hooks/index.ts"),
      },
      formats: ["es"],
    },
    rollupOptions: {
      external: [
        "solid-js",
        /^solid-js\//,
        "@blit-sh/core",
        /^@blit-sh\/core\//,
        "@blit-sh/browser",
      ],
      output: {
        preserveModules: true,
        preserveModulesRoot: "src",
      },
    },
  },
});
