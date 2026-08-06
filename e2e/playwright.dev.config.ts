// Runs the e2e specs against a vite dev server (worktree UI) instead of the
// gateway-embedded bundle.  Server/gateway/vite are managed by the caller.
import { defineConfig } from "@playwright/test";

const BASE_URL = `http://127.0.0.1:${process.env.VITE_PORT || 3298}`;

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  retries: 0,
  workers: 1,
  reporter: "list",
  timeout: 30_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL: BASE_URL,
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: {
        browserName: "chromium",
        // System chromium: the npm-bundled browser doesn't start on NixOS.
        launchOptions: process.env.CHROMIUM_BIN
          ? { executablePath: process.env.CHROMIUM_BIN }
          : {},
      },
    },
  ],
});
