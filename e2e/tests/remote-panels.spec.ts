import { test, expect } from "@playwright/test";

/**
 * Everything a remote has to say lives under that remote.
 *
 * systemd units and extensions used to be status-bar glyphs opening overlays
 * of their own, which put them next to the font size and the audio mute —
 * workspace chrome for things that are properties of one server. They are now
 * tabs of one remote's Manage panel, alongside its applications and clients.
 * This asserts both halves: the glyphs are gone, and the tabs are there.
 */
test("a remote's panels open from its Manage button, not from status-bar glyphs", async ({
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

  // One connected remote opens its own management overlay.
  const control = page.getByRole("button", { name: /Manage/ }).first();
  await expect(control).toBeVisible({ timeout: 5_000 });
  await control.click();
  // Its own dialog, on top of the remotes list rather than inside a row.
  await expect(
    page.locator('[role="dialog"][aria-label^="Manage"]'),
  ).toHaveCount(1);

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

  // Escape closes the management panel and leaves the remotes list standing:
  // one key, one layer.
  await page.keyboard.press("Escape");
  await expect(
    page.locator('[role="dialog"][aria-label^="Manage"]'),
  ).toHaveCount(0);
  await expect(
    page.getByRole("button", { name: /Manage/ }).first(),
  ).toBeVisible();
});
