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
 * An extension is a client, and the clients list now says so.
 *
 * Every running extension holds a connection of its own, so the clients list
 * has always shown it — as `Client 7`, indistinguishable from a browser tab
 * and one click from a Kick that ends the attempt. The server now reports what
 * opened each connection, and this asserts what a viewer sees: the definition's
 * name, an `extension` tag, and a button that says what stopping it does.
 *
 * Installed over the CLI for the same reason as the tab strip's spec: what is
 * under test is what the browser makes of the catalog, not where the module
 * came from.
 */

const BLIT = path.resolve(__dirname, "../../target/debug/blit");
const MODULE = path.resolve(__dirname, "../../extensions/dist/session.wasm");

/** The socket of the server the gateway under test proxies to, or null.
 *
 *  This spec installs an extension, so it refuses to run against a server it
 *  cannot positively identify: the CLI's own resolution would find the
 *  developer's everyday server and mutate that. */
function e2eSocket(): string | null {
  const handoff = path.resolve(__dirname, "../.e2e-socket");
  if (!fs.existsSync(handoff)) return null;
  const sock = fs.readFileSync(handoff, "utf8").trim();
  return sock && fs.existsSync(sock) ? sock : null;
}

test("the clients list names the extension behind a connection", async ({
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
  const definitions = () =>
    blit("ext", "list")
      .trim()
      .split("\n")
      .filter((row: string) => row.trim())
      .map((row: string) => `id:${row.split("\t")[0]}`);
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

  try {
    const installed = blit("ext", "run", "--persist", "session", MODULE).trim();
    expect(installed).toMatch(/^id:[0-9a-f]+$/);

    await page.getByRole("status").click();
    const manage = page.getByRole("button", { name: /^Manage$/ }).first();
    await expect(manage).toBeVisible({ timeout: 5_000 });
    await manage.click();
    await page.locator('[data-connection-tab="clients"]').click();

    // The catalog is a live watch, so the row arrives on its own: the
    // extension's connection may open after the panel does.
    const row = page.getByText("session", { exact: true });
    await expect(row).toBeVisible({ timeout: 15_000 });
    await expect(page.getByText("extension", { exact: true })).toBeVisible();
    // This viewer's own row is still a browser's, which is what says the tag
    // marks one kind of connection rather than decorating every row.
    await expect(page.getByText("this client", { exact: true })).toBeVisible();

    // The button on that row says what the click does. Kicking an extension's
    // connection ends the running attempt rather than disconnecting a peer.
    await expect(
      page.getByRole("button", { name: "Stop attempt" }),
    ).toBeVisible();
    await expect(
      page.getByRole("button", { name: "Kick" }).first(),
    ).toBeVisible({ timeout: 5_000 });
  } finally {
    removeAll();
  }
});
