import { test, expect } from "@playwright/test";
import { execFileSync } from "child_process";
import fs from "fs";
import path from "path";

/**
 * A manage tile is registered in the *host's* open-tab list (docs/design/kv.md),
 * so leaving one open outlives this page: it is the first parked card in every
 * spec that runs after this one, and their own `localStorage.clear()` cannot
 * reach it — a card whose body is a title rather than a preview, which is not
 * what a dock full of terminals is expected to start with. Closing the focused
 * tile is the only thing that unregisters it.
 */
test.afterEach(async ({ page }) => {
  const panels = page.locator("[data-connection-tab]");
  if (
    !(await panels
      .first()
      .isVisible()
      .catch(() => false))
  )
    return;
  await page.keyboard.press("Control+Alt+Shift+q");
  await expect(panels).toHaveCount(0);
});

/**
 * A remote's extension tabs appear and disappear with the extension.
 *
 * Session and systemd are tabs of one server's Manage panel, and each exists
 * only while its extension is serving. That used to be sampled once per
 * expansion, so the panel that installs extensions was one tab away from a tab
 * strip that could not see the install. The server now publishes which channel
 * names have a listener (`CHANNEL_WATCH`), and this asserts the consequence:
 * the strip changes with the panel open, in both directions.
 *
 * The extension is installed over the CLI rather than through the Extensions
 * panel because the panel's registry is a network service in one harness and a
 * local port in the other. What is under test is the browser noticing, not
 * where the module came from.
 */

const BLIT = path.resolve(__dirname, "../../target/debug/blit");
const MODULE = path.resolve(__dirname, "../../extensions/dist/session.wasm");
const SESSION_TAB = '[data-connection-tab="session"]';

/** The socket of the server the gateway under test proxies to, or null.
 *
 *  Unlike the read-only specs, this one installs and removes an extension, so
 *  it refuses to run against a server it cannot positively identify: the CLI's
 *  own resolution would find the developer's everyday server and mutate that. */
function e2eSocket(): string | null {
  const handoff = path.resolve(__dirname, "../.e2e-socket");
  if (!fs.existsSync(handoff)) return null;
  const sock = fs.readFileSync(handoff, "utf8").trim();
  return sock && fs.existsSync(sock) ? sock : null;
}

test("installing an extension adds its tab, removing it takes it away", async ({
  page,
}) => {
  const sock = e2eSocket();
  if (!sock) {
    test.skip(
      true,
      "no e2e server socket to install into (start-servers.sh publishes it)",
    );
  }
  if (!fs.existsSync(MODULE)) {
    test.skip(true, `no session extension at ${MODULE} (run bin/extensions)`);
  }
  const blit = (...args: string[]) =>
    execFileSync(BLIT, ["--on", `socket:${sock}`, ...args], {
      encoding: "utf8",
    });
  /** Every definition this server has, as `id:<hex>` selectors.
   *
   *  Addressed by ID rather than by the name they were installed under: a
   *  server that has seen an earlier run can hold more than one definition
   *  called `session`, and a name that resolves to the wrong one fails in a
   *  way that reads as the feature being broken. */
  const definitions = () =>
    blit("ext", "list")
      .trim()
      .split("\n")
      .filter((row: string) => row.trim())
      .map((row: string) => `id:${row.split("\t")[0]}`);

  /** Remove every extension, so "no Session tab" is a fact rather than a hope.
   *  Removal is a two-step verb — a definition must be quiescent first — and a
   *  transient definition refuses both, which is fine: it holds no channel. */
  const removeAll = () => {
    for (const selector of definitions()) {
      try {
        blit("ext", "disable", selector);
        blit("ext", "remove", selector);
      } catch {
        // Transient, already gone, or not ours to remove.
      }
    }
  };

  removeAll();

  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  // The passphrase is read once at module load, so a reload is what puts the
  // page in the state a returning visitor is in.
  await page.reload();
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });

  await page.getByRole("status").click();
  const manage = page.getByRole("button", { name: /^Manage$/ }).first();
  await expect(manage).toBeVisible({ timeout: 5_000 });
  await manage.click();
  await expect(page.locator("[data-connection-tab]").first()).toBeVisible({
    timeout: 5_000,
  });
  await expect(page.locator(SESSION_TAB)).toHaveCount(0);

  try {
    // Nothing is touched in the browser from here on. The panel stays open the
    // whole time, which is the only version of this that proves anything: a
    // reopened panel would re-ask and pass with the old probe.
    const installed = blit("ext", "run", "--persist", "session", MODULE).trim();
    expect(installed).toMatch(/^id:[0-9a-f]+$/);
    await expect(page.locator(SESSION_TAB)).toHaveCount(1, { timeout: 15_000 });
    await expect(page.locator(SESSION_TAB)).toHaveText("Session");
    // Deliberately not opened: what the supervisor then puts in the panel is
    // its own spec's business, and connecting to it would make this one fail
    // for reasons that have nothing to do with the tab strip.
  } finally {
    removeAll();
  }

  // Going away reaches the strip too. It arrives with the extension's endpoint
  // teardown rather than with the control call, so this is a poll, and the
  // panel it was showing must give way to one that still exists.
  await expect(page.locator(SESSION_TAB)).toHaveCount(0, { timeout: 15_000 });
  await expect(page.locator("[data-connection-tab]").first()).toBeVisible();
});
