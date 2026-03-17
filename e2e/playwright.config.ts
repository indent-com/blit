import { defineConfig } from "@playwright/test";
import path from "path";

const PORT = 3274;
const BASE_URL = `http://127.0.0.1:${PORT}`;

export default defineConfig({
  testDir: "./tests",
  fullyParallel: false,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 2 : 1,
  workers: 1,
  reporter: process.env.CI ? "github" : "list",
  timeout: 30_000,
  expect: {
    timeout: 10_000,
  },
  use: {
    baseURL: BASE_URL,
    trace: "on-first-retry",
    screenshot: "only-on-failure",
  },
  projects: [
    {
      name: "chromium",
      use: { browserName: "chromium" },
    },
  ],
  webServer: {
    command: path.resolve(__dirname, "start-servers.sh"),
    url: BASE_URL,
    reuseExistingServer: !process.env.CI,
    timeout: 30_000,
    env: {
      BLIT_PASSPHRASE: "test-secret",
      BLIT_ADDR: `127.0.0.1:${PORT}`,
    },
    stdout: "pipe",
    stderr: "pipe",
  },
});
