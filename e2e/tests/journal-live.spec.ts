import { test, expect } from "@playwright/test";
import { execFileSync } from "node:child_process";

/**
 * The journal pane is a live tail over history that grows as it is scrolled.
 *
 * Both halves used to be missing: every view was one 200-line page, and reading
 * more of it — in either direction — meant a button. This asserts the two
 * behaviours a log reader assumes it has. The tail is asserted against an entry
 * written after the pane was already open, because a pane that merely *starts*
 * with fresh entries is indistinguishable from a dead one.
 */
test("the journal tails live and pages history in as it scrolls", async ({
  page,
}) => {
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
  const manage = page.getByRole("button", { name: /Manage/ }).first();
  await expect(manage).toBeVisible({ timeout: 5_000 });
  await manage.click();

  // systemd is an extension's tab, and which tabs exist is discovered by
  // asking the channel rather than known up front — so this waits for it to
  // appear, and only calls the server watcher-less once it has not.
  const systemd = page.locator('[data-connection-tab="systemd"]');
  const present = await systemd
    .waitFor({ state: "attached", timeout: 10_000 })
    .then(() => true)
    .catch(() => false);
  if (!present) {
    test.skip(true, "this server runs no systemd watcher extension");
  }
  await systemd.click();
  await page.locator('[data-systemd-tab="logs"]').click();

  // The user journal, because that is the one this test can write to:
  // systemd-cat outside a service lands under the caller's UID, and the
  // pane's default `--system` would never show it.
  await page.locator("[data-journal-scope]").selectOption("user");

  const rows = page.locator("[data-journal-row]");
  await expect(rows.first()).toBeVisible({ timeout: 15_000 });
  const firstPage = await rows.count();
  expect(firstPage).toBeGreaterThan(0);

  // Being live is the default rather than a switch the reader has to find.
  await expect(page.locator("[data-journal-live]")).toHaveAttribute(
    "data-journal-live",
    "on",
  );

  // An entry written now, with nothing touched in the browser. journald is the
  // only writer either side trusts, so this is the honest oracle for "live" —
  // the status text only says what the pane intends.
  const stamp = `blit-e2e-journal-${Date.now()}`;
  try {
    execFileSync("systemd-cat", ["-t", "blit-e2e", "echo", stamp], {
      stdio: "ignore",
    });
  } catch {
    test.skip(true, "no systemd-cat to write a journal entry with");
  }
  await expect(page.getByText(stamp, { exact: false })).toBeVisible({
    timeout: 20_000,
  });

  // Reading back is scrolling, not clicking. Asserted as the *first* row
  // changing rather than the count growing: a live tail appending at the bottom
  // would satisfy a count, and prove nothing about history.
  const list = page.locator("[data-journal-list]");
  const oldestBefore = await rows.first().innerText();
  await list.evaluate((node) => {
    node.scrollTop = 0;
  });
  await expect
    .poll(async () => rows.first().innerText(), { timeout: 15_000 })
    .not.toBe(oldestBefore);
  expect(await rows.count()).toBeGreaterThan(firstPage);
  // The row that was oldest is still there — history was prepended, not
  // swapped for a different page.
  await expect(
    page.getByText(oldestBefore.split("\n")[0]!).first(),
  ).toHaveCount(1);
});
