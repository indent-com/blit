import { defineConfig } from "vitest/config";

// No solid plugin: this suite deliberately covers the app's non-component
// logic — the preview service worker above all, which carries the cookie and
// message-origin rules a previewed page's security depends on. Rendering
// components is @blit-sh/solid's job and needs a DOM harness this does not.
//
// Reactive (non-rendering) logic is still in scope, and needs solid-js to
// resolve to its CLIENT build: under node conditions the package resolves to
// the server build, where createEffect is a no-op and every store/watch hook
// silently does nothing. No JSX is involved, so no plugin is implied.
export default defineConfig({
  resolve: { conditions: ["browser", "development"] },
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
