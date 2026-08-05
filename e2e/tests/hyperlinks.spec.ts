import { test, expect, type Page } from "@playwright/test";

/**
 * OSC 8 hyperlinks, end to end: escape sequence out of a real PTY, through the
 * wire protocol, into the browser's hover preview and activation dialog.
 *
 * The cases that matter are the ones where the link's *text* and its *target*
 * disagree — that is the whole point of OSC 8, and the whole reason the client
 * classifies a target before opening it.
 */

const ESC = "\x1b";

/** Wrap `text` in an OSC 8 hyperlink pointing at `target`. */
function osc8(target: string, text: string): string {
  return `${ESC}]8;;${target}${ESC}\\${text}${ESC}]8;;${ESC}\\`;
}

/**
 * Render a string as printf octal escapes, so the payload survives being typed
 * through a shell without any quoting ambiguity.
 *
 * Escapes are emitted per UTF-8 *byte*, not per codepoint: `\nnn` is a single
 * byte, so a codepoint above U+00FF has to be spelled as its encoded bytes or
 * printf silently truncates the escape and writes something else entirely.
 */
function toPrintfOctal(s: string): string {
  return [...new TextEncoder().encode(s)]
    .map((b) => {
      const literal = b >= 0x20 && b <= 0x7e && b !== 0x5c && b !== 0x25;
      return literal
        ? String.fromCharCode(b)
        : "\\" + b.toString(8).padStart(3, "0");
    })
    .join("");
}

async function authenticateAndCreateTerminal(page: Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  await page.waitForTimeout(2000);

  // The remotes overlay auto-opens on a fresh profile and swallows clicks.
  for (let i = 0; i < 3; i++) {
    const dialog = page.locator('[role="dialog"]').first();
    if (!(await dialog.isVisible().catch(() => false))) break;
    await page.keyboard.press("Escape");
    await page.waitForTimeout(300);
  }

  // `.blit-scroll-surface` is the element that actually receives pointer
  // events; the canvas beneath it is not hit-testable.
  const surface = page.locator(".blit-scroll-surface").first();
  if (!(await surface.isVisible().catch(() => false))) {
    // The empty state's button opens a target/command prompt rather than
    // creating a terminal outright, so the Enter is load-bearing.
    await page.getByRole("button", { name: "New terminal" }).first().click();
    await page.waitForTimeout(500);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(1000);
  }
  if (!(await surface.isVisible().catch(() => false))) {
    await page.keyboard.press("Control+Enter");
  }
  await surface.waitFor({ state: "visible", timeout: 20_000 });
  // The shell needs to be up before anything is typed at it.
  await page.waitForTimeout(1500);
  return surface;
}

/** Type a payload into the focused terminal and wait for it to render. */
async function printToTerminal(page: Page, payload: string) {
  const inputSink = page.locator(
    'textarea[aria-label="Terminal input"]:not([readonly])',
  );
  await inputSink.focus();
  await page.keyboard.type("clear");
  await page.keyboard.press("Enter");
  await page.waitForTimeout(500);
  await page.keyboard.type(`printf '${toPrintfOctal(payload)}'`);
  await page.keyboard.press("Enter");
  await page.waitForTimeout(1500);
}

/** Whatever target the status bar is currently previewing, if any. */
function hoveredTarget(page: Page): Promise<string | null> {
  return page.evaluate(() => {
    const footer = document.querySelector("footer");
    if (!footer) return null;
    const hit = [...footer.querySelectorAll("span")].find((s) =>
      /^(https?:|javascript:|slack:|file:|mailto:)/.test(
        (s.textContent ?? "").trim(),
      ),
    );
    return hit ? hit.textContent!.trim() : null;
  });
}

/**
 * Sweep down the left edge of the grid until the status bar previews a link,
 * returning the y offset that hit it. Avoids having to derive cell metrics.
 */
async function findLinkRow(
  page: Page,
  box: { x: number; y: number; width: number; height: number },
  expected: string,
): Promise<number | null> {
  for (let dy = 4; dy < box.height - 20; dy += 5) {
    await page.mouse.move(box.x + 20, box.y + dy);
    await page.waitForTimeout(60);
    if ((await hoveredTarget(page)) === expected) return dy;
  }
  return null;
}

test.describe("OSC 8 hyperlinks", () => {
  test("hovering previews the real target, not the visible text", async ({
    page,
  }) => {
    const surface = await authenticateAndCreateTerminal(page);
    // Text claims to be a bank; the target is a script URL.
    await printToTerminal(
      page,
      osc8("javascript:alert(1)", "https://your-bank.example") + "\n",
    );

    const box = (await surface.boundingBox())!;
    const dy = await findLinkRow(page, box, "javascript:alert(1)");
    expect(dy, "status bar should preview the OSC 8 target").not.toBeNull();
  });

  test("a script-scheme target is blocked with no way to open it", async ({
    page,
  }) => {
    const surface = await authenticateAndCreateTerminal(page);
    await printToTerminal(
      page,
      osc8("javascript:alert(1)", "https://your-bank.example") + "\n",
    );

    const box = (await surface.boundingBox())!;
    const dy = await findLinkRow(page, box, "javascript:alert(1)");
    expect(dy).not.toBeNull();

    await page.keyboard.down("Alt");
    await page.mouse.click(box.x + 20, box.y + dy!);
    await page.keyboard.up("Alt");

    const dialog = page.locator('[role="dialog"][aria-label="Link"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });

    const text = await dialog.innerText();
    expect(text).toContain("Link blocked");
    // Both sides of the deception are shown, so the mismatch is legible.
    expect(text).toContain("https://your-bank.example");
    expect(text).toContain("javascript:alert(1)");

    // Crucially: dismissal is the only action offered.
    const buttons = await dialog.locator("button").allTextContents();
    expect(buttons.map((b) => b.trim())).not.toContain("Open link");
  });

  test("a custom scheme prompts before opening", async ({ page }) => {
    const surface = await authenticateAndCreateTerminal(page);
    await printToTerminal(page, osc8("slack://team/general", "chat") + "\n");

    const box = (await surface.boundingBox())!;
    const dy = await findLinkRow(page, box, "slack://team/general");
    expect(dy).not.toBeNull();

    await page.keyboard.down("Alt");
    await page.mouse.click(box.x + 20, box.y + dy!);
    await page.keyboard.up("Alt");

    const dialog = page.locator('[role="dialog"][aria-label="Link"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog).toContainText("Open this link?");
    await expect(dialog).toContainText("slack://team/general");

    const buttons = (await dialog.locator("button").allTextContents()).map(
      (b) => b.trim(),
    );
    expect(buttons).toContain("Open link");
    expect(buttons).toContain("Cancel");

    // Escape must mean "do not open".
    await page.keyboard.press("Escape");
    await expect(dialog).toBeHidden({ timeout: 5_000 });
  });

  test("an ordinary https target opens without a prompt", async ({ page }) => {
    const surface = await authenticateAndCreateTerminal(page);
    await printToTerminal(
      page,
      osc8("https://blit.sh/docs", "the docs") + "\n",
    );

    const box = (await surface.boundingBox())!;
    const dy = await findLinkRow(page, box, "https://blit.sh/docs");
    expect(dy).not.toBeNull();

    // Stub window.open rather than letting a real tab open: it keeps the run
    // hermetic, and asserting the argument is a stronger check than merely
    // observing that no dialog appeared.
    await page.evaluate(() => {
      (window as unknown as { __opened: string[] }).__opened = [];
      window.open = ((url?: string | URL) => {
        (window as unknown as { __opened: string[] }).__opened.push(
          String(url),
        );
        return null;
      }) as typeof window.open;
    });

    await page.keyboard.down("Alt");
    await page.mouse.click(box.x + 20, box.y + dy!);
    await page.keyboard.up("Alt");
    await page.waitForTimeout(800);

    await expect(
      page.locator('[role="dialog"][aria-label="Link"]'),
    ).toHaveCount(0);
    const opened = await page.evaluate(
      () => (window as unknown as { __opened: string[] }).__opened,
    );
    expect(opened).toEqual(["https://blit.sh/docs"]);
  });

  test("a target with hidden characters is blocked", async ({ page }) => {
    const surface = await authenticateAndCreateTerminal(page);
    // U+202E flips rendering of everything after it, so the displayed target
    // cannot be trusted to match where the link goes.
    const target = "https://example.com/‮gnp.exe";
    await printToTerminal(page, osc8(target, "report") + "\n");

    const box = (await surface.boundingBox())!;
    let dy: number | null = null;
    for (let y = 4; y < box.height - 20 && dy === null; y += 5) {
      await page.mouse.move(box.x + 20, box.y + y);
      await page.waitForTimeout(60);
      const t = await hoveredTarget(page);
      if (t?.startsWith("https://example.com/")) dy = y;
    }
    expect(dy, "hidden-character link should still preview").not.toBeNull();

    // The preview must escape the invisible codepoint rather than obey it.
    const preview = await hoveredTarget(page);
    expect(preview).toContain("<U+202E>");

    await page.keyboard.down("Alt");
    await page.mouse.click(box.x + 20, box.y + dy!);
    await page.keyboard.up("Alt");

    const dialog = page.locator('[role="dialog"][aria-label="Link"]');
    await expect(dialog).toBeVisible({ timeout: 5_000 });
    await expect(dialog).toContainText("Link blocked");
    const buttons = await dialog.locator("button").allTextContents();
    expect(buttons.map((b) => b.trim())).not.toContain("Open link");
  });
});
