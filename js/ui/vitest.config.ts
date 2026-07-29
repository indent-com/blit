import { defineConfig } from "vitest/config";

// No solid plugin: this suite deliberately covers the app's non-component
// logic — the preview service worker above all, which carries the cookie and
// message-origin rules a previewed page's security depends on. Rendering
// components is @blit-sh/solid's job and needs a DOM harness this does not.
export default defineConfig({
  test: {
    environment: "jsdom",
    globals: true,
    include: ["src/**/__tests__/**/*.test.ts"],
    coverage: {
      provider: "v8",
      include: ["src/sw/**/*.ts"],
      exclude: ["src/**/__tests__/**"],
      reporter: ["text", "html"],
      reportsDirectory: "coverage",
    },
  },
});
