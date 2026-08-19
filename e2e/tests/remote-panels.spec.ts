import { test, expect } from "@playwright/test";

/**
 * Everything a remote has to say lives under that remote — as a pane.
 *
 * systemd units and extensions used to be status-bar glyphs opening overlays
 * of their own, which put them next to the font size and the audio mute —
 * workspace chrome for things that are properties of one server. They are now
 * tabs of one remote's Manage tile, alongside its applications and clients.
 *
 * And a tile rather than a dialog because a dialog could not survive being
 * used: enabling an application in the Session tab starts it, a fresh window
 * asks to be raised, and an activation closes whatever overlay is up. So this
 * asserts three things: the glyphs are gone, Manage opens pane content (the
 * remotes dialog closing behind it), and the panels are still there after the
 * click that used to dismiss them.
 */
test("a remote's panels open as a pane from its Manage button, not from status-bar glyphs", async ({
  page,
}) => {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  // The passphrase is read once at module load, before this hash existed, and
  // the gateway config connection is what the remotes list comes from. A
  // reload is what puts the page in the state a returning visitor is in.
  await page.reload();
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
  await page.waitForTimeout(500);

  // The two retired status-bar entries. Located by key rather than by glyph:
  // the workspace-roots control is a ⚙ too, and it is not going anywhere.
  await expect(page.locator('[data-status-tool="systemd"]')).toHaveCount(0);
  await expect(page.locator('[data-status-tool="extensions"]')).toHaveCount(0);

  // The connection-status indicator is what opens the remotes panel.
  await page.getByRole("status").click();
  await expect(page.getByText("Remotes", { exact: true }).first()).toBeVisible({
    timeout: 5_000,
  });

  // Exactly "Manage", not merely containing it: a parked manage tile's dock
  // card is a button too, and its accessible name starts with the remote's
  // name and ends with the word — which a /Manage/ locator matches, under a
  // modal backdrop, unclickably.
  const control = page.getByRole("button", { name: /^Manage$/ }).first();
  await expect(control).toBeVisible({ timeout: 5_000 });
  await control.click();

  // Pane content, and the dialog that asked for it is gone rather than
  // stacked under a second one.
  await expect(page.locator('[role="dialog"]')).toHaveCount(0);

  // Clients is the tab every connected server can offer; the extension-backed
  // ones appear only where their channel answers, so this asserts the strip
  // exists and that clients is in it rather than a fixed set.
  const tabs = page.locator("[data-connection-tab]");
  await expect(tabs.first()).toBeVisible({ timeout: 5_000 });
  await expect(page.locator('[data-connection-tab="clients"]')).toHaveCount(1);

  // Extensions is a server capability rather than an installed extension, so
  // it is present here, and its registry defaults to the dev stack's own —
  // three ports up from the page, which is what bin/dev allocates.
  const extensions = page.locator('[data-connection-tab="extensions"]');
  await expect(extensions).toHaveCount(1);
  await extensions.click();
  const registry = page.locator("[data-registry-url]");
  await expect(registry).toBeVisible({ timeout: 5_000 });
  // Whichever registry this page can actually reach. Under `vite dev` the
  // stack's own is proxied at /ext on the page's origin; the gateway serves a
  // production bundle with no proxy in front of it and points at the published
  // one. Both harnesses run this spec, so it asserts the choice, not a port.
  const origin = new URL(page.url()).origin;
  await expect(registry).toHaveValue(
    new RegExp(
      `^(${origin.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}|https://install\\.blit\\.sh)/ext$`,
    ),
  );

  // A tile is addressed by the URL like every other one, so a reload brings
  // the panels back rather than the workspace it replaced.
  await expect
    .poll(() => page.evaluate(() => location.hash), { timeout: 5_000 })
    .toMatch(/tile=/);
  await page.reload();
  await expect(page.locator("[data-connection-tab]").first()).toBeVisible({
    timeout: 10_000,
  });
});
