import { test, expect, type Page } from "@playwright/test";
import { execFileSync } from "node:child_process";

/**
 * The Branches panel: `git branch` plus the worktrees, in the left dock.
 *
 * Driven against a fixture repository the caller set up as `BRANCHES_REPO`
 * (three local branches, a remote with `origin/HEAD`, one commit ahead of it,
 * three tags, four worktrees — one detached, one locked). The point of doing
 * this in a browser rather than in unit tests is the two claims no unit test
 * reaches: that the worktree list arrives over the wire at all, and that it is
 * *live* — a `git worktree add` in the fixture has to show up without a
 * reload, which is the whole reason `WORKTREE_GEN` exists.
 */

const REPO = process.env.BRANCHES_REPO ?? "";
const WT_DIR = process.env.BRANCHES_WT ?? "";

function git(args: string[], cwd = REPO): string {
  return execFileSync("git", args, {
    cwd,
    encoding: "utf8",
    env: { ...process.env, GIT_CONFIG_GLOBAL: "/dev/null" },
  });
}

/** Open the workspace, authenticate, and reveal the Branches section. */
async function openPanel(page: Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  // A reload after the passphrase lands, or the first connect races it.
  await page.reload();
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 20_000 });
  await page.waitForTimeout(1500);

  // A terminal is what gives the dock a root at all: the server was started
  // in the fixture repo, so the pty's cwd is what every git-backed section
  // anchors on. Without one the panel correctly folds itself away on "no
  // repository", which is not what these tests are about.
  const canvas = page.locator("canvas").first();
  if (!(await canvas.isVisible().catch(() => false))) {
    await page.getByRole("button", { name: "New terminal" }).first().click();
  }
  await expect(canvas).toBeVisible({ timeout: 20_000 });
  // Give the pty's cwd poll and the repo open time to land: every git-backed
  // section is empty until the repo behind them is actually open.
  await page.waitForTimeout(3000);

  // The dock section title comes from i18n as "Branches"; the dock uppercases
  // it in CSS, so the DOM text is the mixed-case string.
  const header = page.getByText("Branches", { exact: true });
  if (!(await header.isVisible().catch(() => false))) {
    await page.keyboard.press("Control+Shift+Y");
  }
  await expect(header).toBeVisible({ timeout: 15_000 });
  // Expand it if it is folded (a fresh profile starts most sections shut).
  if (
    !(await page
      .getByText("WORKTREES", { exact: true })
      .isVisible()
      .catch(() => false))
  ) {
    await header.click();
  }
  await expect(page.getByText("WORKTREES", { exact: true })).toBeVisible({
    timeout: 15_000,
  });
}

/** The panel's worktree rows, by label. Keyed off the row's own
 *  `data-worktree`, not its tooltip: a branch checked out elsewhere names a
 *  worktree path in its title too, so a title match cannot tell them apart. */
async function worktreeNames(page: Page): Promise<string[]> {
  return page.locator("[data-worktree]").allInnerTexts();
}

test("lists branches with HEAD, divergence, and the remote default", async ({
  page,
}) => {
  test.skip(!REPO, "BRANCHES_REPO not set");
  await openPanel(page);

  // Local branches, `main` first, with git's own `*` on HEAD.
  const local = page.locator('[data-branch^="refs/heads/"]');
  await expect(local.first()).toBeVisible({ timeout: 15_000 });
  const labels = await local.allInnerTexts();
  expect(labels.join(" ")).toContain("main");
  expect(labels.join(" ")).toContain("feature");
  expect(labels.join(" ")).toContain("pinned");

  // HEAD is on main and marked with the `*` column.
  const mainRow = page.locator('[data-branch="refs/heads/main"]').first();
  await expect(mainRow).toContainText("*");
  await expect(mainRow).toContainText("main");
  // One commit ahead of origin/main, which the fixture arranged.
  expect(git(["rev-list", "--count", "origin/main..main"]).trim()).toBe("1");
  await expect(mainRow).toContainText("↑1");
  // …and its tooltip names the upstream it is ahead of.
  expect(await mainRow.getAttribute("title")).toContain(
    "tracks refs/remotes/origin/main",
  );

  // Remotes and tags fold away — a repo can have far more tags than branches.
  await expect(page.getByText("REMOTE", { exact: true })).toBeVisible();
  await expect(page.getByText("TAGS", { exact: true })).toBeVisible();
  await expect(page.locator('[data-branch^="refs/remotes/"]')).toHaveCount(0);

  await page.getByText("REMOTE", { exact: true }).click();
  const remote = page.locator('[data-branch^="refs/remotes/"]');
  await expect(remote.first()).toBeVisible();
  const remoteLabels = (await remote.allInnerTexts()).join(" ");
  // origin/HEAD is a symref, not a branch: it marks the default instead of
  // getting a row of its own.
  expect(remoteLabels).toContain("origin/main");
  expect(remoteLabels).not.toContain("origin/HEAD");
  expect(remoteLabels).toContain("default");

  // Tags sort numerically-descending, so v10 sits above v2 and v9.
  await page.getByText("TAGS", { exact: true }).click();
  const tags = page.locator('[data-branch^="refs/tags/"]');
  await expect(tags.first()).toBeVisible();
  // The fixture tags v1, v2, v10. A plain string sort would give v1 v10 v2.
  expect(await tags.allInnerTexts()).toEqual(["v10", "v2", "v1"]);
});

test("lists worktrees, marks the current one, and shows lock + detach", async ({
  page,
}) => {
  test.skip(!REPO, "BRANCHES_REPO not set");
  await openPanel(page);

  await expect(page.getByText("WORKTREES", { exact: true })).toBeVisible();
  // Four, matching `git worktree list` exactly.
  const expected = git(["worktree", "list", "--porcelain"])
    .split("\n\n")
    .filter((s) => s.trim()).length;
  expect(expected).toBe(4);
  const names = await worktreeNames(page);
  expect(names.length).toBe(4);

  const joined = names.join(" | ");
  // The main worktree is labelled as such and is the one we are open at.
  expect(joined).toContain("repo");
  expect(joined).toContain("main");
  expect(joined).toContain("feature");
  // A detached worktree names no branch — it shows its commit instead.
  expect(joined).toContain("detached");
  // The locked one carries its reason on the row's tooltip.
  const pinned = page.locator('[data-worktree$="/pinned"]').first();
  expect(await pinned.getAttribute("title")).toContain("on usb");
});

test("clicking a branch retargets the commit log", async ({ page }) => {
  test.skip(!REPO, "BRANCHES_REPO not set");
  await openPanel(page);

  // The log spec input is the observable: the panel does not keep its own
  // selection, it reflects what the Log panel is walking.
  const spec = page.locator('input[placeholder*="HEAD"]').first();
  await page.locator('[data-branch="refs/heads/feature"]').first().click();
  await expect(spec).toHaveValue("refs/heads/feature", { timeout: 10_000 });

  await page.locator('[data-branch="refs/heads/main"]').first().click();
  await expect(spec).toHaveValue("refs/heads/main");
});

test("clicking a worktree re-roots the dock", async ({ page }) => {
  test.skip(!REPO || !WT_DIR, "fixture env not set");
  await openPanel(page);

  // Before: the root picker is on the focused pane, and the file tree shows
  // the main worktree.
  const picker = page.locator('select[title="Workspace root"]');
  await expect(picker).toHaveValue("__focused__");

  const featureWt = `${WT_DIR}/feature`;
  await page.locator(`[data-worktree="${featureWt}"]`).click();

  // The picker now names the worktree, as a place we went rather than a
  // configured root.
  await expect(picker).toHaveValue("__worktree__", { timeout: 15_000 });
  await expect(picker.locator("option[value='__worktree__']")).toContainText(
    "feature",
  );

  // And the panel now marks *that* worktree as current — proof the whole dock
  // re-rooted and re-opened the repository there, not just the label. The
  // server decides `CURRENT` from the repo handle's own worktree, so this
  // could not be faked client-side by the click that requested it.
  await expect(page.locator(`[data-worktree="${featureWt}"]`)).toContainText(
    "●",
    { timeout: 15_000 },
  );
  // …and the one we left no longer claims it.
  await expect(page.locator(`[data-worktree="${REPO}"]`)).not.toContainText(
    "●",
  );
});

/**
 * The claim that a one-shot request cannot make on its own: adding and
 * removing a worktree in the fixture reaches an already-open panel with no
 * reload. `git worktree remove` moves no ref, so this fails outright if the
 * generation is not in the state stream.
 */
test("the worktree list is live", async ({ page }) => {
  test.skip(!REPO || !WT_DIR, "fixture env not set");
  await openPanel(page);
  await expect(page.getByText("WORKTREES", { exact: true })).toBeVisible();
  expect((await worktreeNames(page)).length).toBe(4);

  const added = `${WT_DIR}/live-probe`;
  try {
    git(["worktree", "add", "-b", "live-probe", added]);
    // No reload, no click: the panel has to hear about this by itself.
    await expect
      .poll(async () => (await worktreeNames(page)).join(" | "), {
        timeout: 20_000,
      })
      .toContain("live-probe");

    // Removal is the half a ref-move refetch could never see.
    git(["worktree", "remove", added]);
    await expect
      .poll(async () => (await worktreeNames(page)).join(" | "), {
        timeout: 20_000,
      })
      .not.toContain("live-probe");
    // The branch it created outlives the worktree — which is exactly why a
    // ref-move signal would have been blind to the removal.
    expect(
      git(["rev-parse", "--verify", "refs/heads/live-probe"]).trim(),
    ).toHaveLength(40);
  } finally {
    // `prune` rather than `remove`: on the happy path the worktree is already
    // gone, and `remove` would print a "not a working tree" fatal into the
    // test output for a cleanup that had nothing to do.
    try {
      git(["worktree", "prune"]);
    } catch {}
    try {
      git(["branch", "-D", "live-probe"]);
    } catch {}
  }
});
