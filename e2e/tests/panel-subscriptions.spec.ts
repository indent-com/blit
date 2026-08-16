import { test, expect, type Page } from "@playwright/test";
import { execFileSync } from "child_process";
import path from "path";

/**
 * Closing a side panel drops the subscriptions only that panel needed.
 *
 * The oracle is the server's own client catalog (`blit client list`), not
 * anything the browser reports about itself: the point of the change is that
 * the server stops being asked for frames, and only the server can say
 * whether it still is. A parked terminal's thumbnail lives in the right-hand
 * preview panel, so with that panel closed its pty must not be subscribed.
 *
 * The terminals are created over the CLI rather than through the UI on
 * purpose. Creating one in the browser first asks *which remote* whenever the
 * developer running this has more than one configured, and that picker is not
 * what is under test.
 */

const BLIT = path.resolve(__dirname, "../../target/debug/blit");

function blit(...args: string[]): string {
  return execFileSync(BLIT, args, { encoding: "utf8", env: process.env });
}

/** Terminal subscriptions per client row, e.g. ["1:80x24", "2:?"].
 *  `blit client list` filters its own short-lived connection, so the browser
 *  is the only row that can carry one. */
function subscribedTerminals(): string[] {
  const rows = blit("client", "list").trim().split("\n").slice(1);
  const terminals: string[] = [];
  for (const row of rows) {
    // ID, AGE_S, OUT_BYTES_S, IN_BYTES_S, SUBSCRIPTIONS, TERMINALS, SURFACES
    const field = row.split("\t")[5] ?? "";
    if (field) terminals.push(...field.split(","));
  }
  return terminals.sort();
}

/** Close every terminal, so leftovers from an earlier run cannot be counted. */
function closeAllTerminals() {
  for (const row of blit("terminal", "list").trim().split("\n").slice(1)) {
    const id = row.split("\t")[0];
    if (id) blit("terminal", "close", id);
  }
}

/** Load the workspace with an explicit panel state. `d=l,r` opens both side
 *  panels and `d=l` leaves only the left dock, so the preview panel is the one
 *  variable. Both values are non-empty on purpose: an empty `d=` does not read
 *  back as "everything closed", it falls through to the stored preference. */
async function open(page: Page, panels: string) {
  await page.goto(`/#psk=test-secret&d=${panels}`);
  // The panel state is read from the hash at startup. Navigating between two
  // URLs that differ only in their hash does not reload, so without this the
  // panels keep whatever state the previous step left them in — which looks
  // exactly like a subscription that failed to drop.
  await page.reload();
  await expect(page.locator("canvas").first()).toBeVisible({
    timeout: 15_000,
  });
  // A previous run's overlay can be restored from storage and would sit over
  // the panel; Esc closes whichever one it is.
  await page.keyboard.press("Escape");
  await page.waitForTimeout(500);
}

test.describe("side panel subscriptions", () => {
  test("a parked terminal is unsubscribed while the preview panel is closed", async ({
    page,
  }) => {
    closeAllTerminals();
    // Two terminals is the cheapest setup that parks one: outside BSP mode
    // every session but the focused one is off-screen.
    blit("terminal", "start", "--", "cat");
    blit("terminal", "start", "--", "cat");

    await page.goto("/");
    await page.evaluate(() => localStorage.clear());

    // Preview panel open: both the focused pane and the parked card are
    // watching, so the server sees two terminal subscriptions.
    await open(page, "l,r");
    await expect.poll(subscribedTerminals, { timeout: 15_000 }).toHaveLength(2);

    // Closed: the parked thumbnail is gone, so its stream must go with it.
    // Only the focused terminal is left.
    await open(page, "l");
    await expect.poll(subscribedTerminals, { timeout: 15_000 }).toHaveLength(1);

    // Reopening resubscribes, so the drop is a lease and not a one-way latch.
    await open(page, "l,r");
    await expect.poll(subscribedTerminals, { timeout: 15_000 }).toHaveLength(2);
  });
});
