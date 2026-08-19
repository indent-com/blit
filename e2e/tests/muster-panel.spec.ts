import { test, expect, type Page } from "@playwright/test";
import { execFileSync } from "child_process";
import fs from "fs";
import path from "path";

/**
 * The Muster tab, end to end: the tree a supervisor publishes on
 * `blit.muster.v1` is what the panel draws, and it keeps drawing the right
 * thing when the supervisor is driven from the other side.
 *
 * The nesting is what this asserts, rather than a row count, because the
 * nesting is the part that could not be built from anything else on the wire:
 * a stack's units are grouped under the instance that expanded them, and a
 * window is attributed to a unit by the stamp the compositor put on the socket
 * its terminal was given (`docs/design/muster.md`).
 *
 * It brings its own units. `start-servers.sh` points the supervisor at an empty
 * directory of its own (`BLIT_MUSTER_DIR`) and publishes the path, so this
 * writes fixtures there rather than reading whatever the developer running it
 * happens to supervise — which would mean starting their work.
 */

const BLIT = path.resolve(__dirname, "../../target/debug/blit");
const MODULE = path.resolve(__dirname, "../../extensions/dist/muster.wasm");
const MUSTER_TAB = '[data-connection-tab="muster"]';

/** The socket of the server behind the gateway under test, or null.
 *
 *  Like `extension-tabs.spec.ts`, this one installs an extension and writes
 *  files, so it refuses to run against a server it cannot positively identify:
 *  the CLI's own resolution would find the developer's everyday server. */
function e2eSocket(): string | null {
  const handoff = path.resolve(__dirname, "../.e2e-socket");
  if (!fs.existsSync(handoff)) return null;
  const sock = fs.readFileSync(handoff, "utf8").trim();
  return sock && fs.existsSync(sock) ? sock : null;
}

/** The directory that server's supervisor reads, or null. */
function musterDir(): string | null {
  const handoff = path.resolve(__dirname, "../.e2e-muster-dir");
  if (!fs.existsSync(handoff)) return null;
  const dir = fs.readFileSync(handoff, "utf8").trim();
  return dir && fs.existsSync(dir) ? dir : null;
}

const UNITS: Record<string, unknown> = {
  "clock.json": {
    description: "A clock that never stops",
    command: ["sh", "-c", "while :; do date; sleep 1; done"],
  },
  "dev/stack.json": { description: "A greeter stack", vars: { who: {} } },
  "dev/greeter.json": {
    description: "Greets ${who}",
    command: ["sh", "-c", "while :; do echo hi ${who}; sleep 2; done"],
  },
  "main.json": { stack: "dev", vars: { who: "world" } },
};

function write(dir: string, name: string, body: unknown): void {
  const file = path.join(dir, name);
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify(body));
}

async function openMusterTab(page: Page): Promise<void> {
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
  ).toBeVisible({ timeout: 15_000 });
  await page.getByRole("status").click();
  const manage = page.getByRole("button", { name: /^Manage$/ }).first();
  await expect(manage).toBeVisible({ timeout: 5_000 });
  await manage.click();
  // Channel presence is followed rather than sampled, so the tab arrives after
  // the strip does.
  const tab = page.locator(MUSTER_TAB);
  await tab.waitFor({ state: "attached", timeout: 15_000 });
  await tab.click();
}

test.describe("muster panel", () => {
  const sock = e2eSocket();
  const dir = musterDir();
  const blit = (...args: string[]) =>
    execFileSync(BLIT, ["--on", `socket:${sock}`, ...args], {
      encoding: "utf8",
    });

  test.beforeAll(() => {
    test.skip(
      !sock || !dir,
      "no e2e server to supervise (start-servers.sh publishes both handoffs)",
    );
    test.skip(!fs.existsSync(MODULE), `no muster extension at ${MODULE}`);
    for (const [name, body] of Object.entries(UNITS)) write(dir!, name, body);
    blit("ext", "run", "--persist", "muster", MODULE);
  });

  test.afterAll(() => {
    if (!sock || !dir) return;
    for (const row of blit("ext", "list").trim().split("\n")) {
      const id = row.split("\t")[0];
      if (!id) continue;
      try {
        blit("ext", "disable", `id:${id}`);
        blit("ext", "remove", `id:${id}`);
      } catch {
        // Transient, already gone, or not ours to remove.
      }
    }
    // The fixtures go, the directory stays: it is what the handoff names and
    // what the running supervisor is watching, and removing it would make a
    // second run of this file skip itself.
    for (const entry of fs.readdirSync(dir)) {
      fs.rmSync(path.join(dir, entry), { recursive: true, force: true });
    }
  });

  /** A manage tile registers in the host's open-tab list, so one left open is
   *  the first parked card in every later spec. Closing the focused tile is the
   *  only thing that unregisters it. */
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

  test("nests a stack's units under the instance that expanded them", async ({
    page,
  }) => {
    await openMusterTab(page);

    await expect(page.locator('[data-muster-unit="clock"]')).toBeVisible({
      timeout: 15_000,
    });
    const member = page.locator('[data-muster-unit="main/greeter"]');
    await expect(member).toBeVisible();
    // A member row shows the template name; the instance is the heading above.
    await expect(member).toContainText("greeter");
    await expect(member).not.toContainText("main/greeter");

    // Expanding is what reveals the terminal a unit is running in.
    await member.click();
    await expect(member).toHaveAttribute("aria-expanded", "true");
    await expect(page.getByText(/terminal #\d+/).first()).toBeVisible();

    // The filter reaches into instances rather than matching only their names.
    await page.getByPlaceholder("Filter units…").fill("clock");
    await expect(page.locator('[data-muster-unit="clock"]')).toBeVisible();
    await expect(member).toHaveCount(0);
  });

  test("follows the supervisor, and drives it, without a reload", async ({
    page,
  }) => {
    await openMusterTab(page);
    const clock = page
      .locator('[data-muster-unit="clock"]')
      .locator("xpath=..");
    await expect(clock).toContainText("running", { timeout: 15_000 });

    // Stopped from the CLI: the panel has to learn about it on the channel.
    blit("@muster", "stop", "clock");
    await expect(clock).toContainText("held", { timeout: 10_000 });

    // Started from the panel: the command goes the other way on the same
    // channel, and the state frame is the acknowledgement.
    await clock.getByRole("button", { name: "Start" }).click();
    await expect(clock).toContainText("running", { timeout: 10_000 });

    // A unit file appearing is a reload the panel is told about, not one it
    // polls for — the supervisor watches the directory and republishes.
    write(dir!, "late.json", {
      description: "late",
      command: ["sleep", "300"],
    });
    await expect(page.locator('[data-muster-unit="late"]')).toBeVisible({
      timeout: 15_000,
    });
    fs.rmSync(path.join(dir!, "late.json"));
    await expect(page.locator('[data-muster-unit="late"]')).toHaveCount(0, {
      timeout: 15_000,
    });
  });

  test("backfills the journal on connect", async ({ page }) => {
    await openMusterTab(page);
    await page.locator('[data-muster-tab="journal"]').click();
    // Already true before anything happens: the supervisor hands a new reader
    // its journal tail, because that is the one thing a state frame cannot say.
    const journal = page.locator("[data-muster-journal]");
    await expect(journal).toContainText("clock", { timeout: 15_000 });
    await expect(journal).toContainText(/started|loaded/);
  });
});
