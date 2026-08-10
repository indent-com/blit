import { test, expect, type Page } from "@playwright/test";

/**
 * The software keyboard is toggle-only, and the key line tracks the keyboard.
 *
 * Two properties, both directions each:
 *  - Tapping a terminal focuses it but must NOT raise an IME: the textarea
 *    carries inputmode="none" until the status-bar toggle is hit, which
 *    removes it (the browser owns the actual IME decision; the attribute is
 *    the whole contract we can pin from here).
 *  - The extra-keys line appears only while a keyboard actually occludes the
 *    viewport, and vanishes the moment it is reduced — not a settling period
 *    later.  The keyboard is emulated by shrinking the device metrics with
 *    the width held constant, which is exactly the signal the occlusion
 *    tracker reads off visualViewport.
 */
test.use({
  hasTouch: true,
  isMobile: true,
  viewport: { width: 480, height: 800 },
});

async function authenticate(page: Page) {
  await page.goto("/");
  await page.evaluate(() => localStorage.clear());
  await page.goto("/#psk=test-secret");
  // A hash-only navigation stores the psk but does not reliably (re)connect
  // the config WS — only the first context in a fresh browser gets away
  // without a real load.  Reload so the boot path picks the stored psk up.
  await page.reload();
  await expect(
    page
      .getByRole("button", { name: "New terminal" })
      .first()
      .or(page.locator("canvas").first()),
  ).toBeVisible({ timeout: 10_000 });
  await page.keyboard.press("Escape");
  // Terminal creation goes over the mux; wait for it, not just the UI shell.
  await expect(page.getByRole("status")).toHaveAttribute(
    "aria-label",
    "connected",
    { timeout: 15_000 },
  );
  await page.waitForTimeout(500);
}

async function newTerminal(page: Page) {
  const before = await page.locator("canvas").count();
  const button = page.getByRole("button", { name: "New terminal" }).first();
  if (await button.isVisible()) {
    await button.click();
  } else {
    await page.keyboard.press("ControlOrMeta+Enter");
  }
  await expect
    .poll(() => page.locator("canvas").count(), { timeout: 10_000 })
    .toBeGreaterThan(before);
  await page.waitForTimeout(300);
}

test("keyboard rises only from the toggle and the key line tracks it", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  const input = page.locator('textarea[aria-label="Terminal input"]').first();
  const keyLine = page.getByRole("button", { name: "Esc" });

  // A terminal on a touch device suppresses the IME from the start.
  await expect(input).toHaveAttribute("inputmode", "none");

  // Tapping the terminal takes focus — for hardware keys and scrollback —
  // but keeps the IME suppressed and raises no key line.
  await page.locator(".blit-scroll-surface").first().tap();
  await expect(input).toBeFocused();
  await expect(input).toHaveAttribute("inputmode", "none");
  await expect(keyLine).toHaveCount(0);

  // The status-bar toggle is the one thing that clears the suppression.
  await page.getByTitle("Show keyboard").tap();
  await expect(input).not.toHaveAttribute("inputmode", "none");
  // Intent alone does not show the key line; the keyboard has not risen.
  await expect(keyLine).toHaveCount(0);

  // The keyboard rising = the visual viewport shrinking under a constant
  // width.  The key line appears with it.
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toBeVisible();

  // Extra keys travel through the terminal's keyboard path, where cursor
  // application mode and one-shot modifiers are encoded. Raw workspace bytes
  // bypassed that state and made the row fail inside TUIs.
  await page.evaluate(() => {
    const input = document.querySelector<HTMLTextAreaElement>(
      'textarea[aria-label="Terminal input"]',
    );
    if (!input) throw new Error("no terminal input");
    const log: string[] = [];
    (
      window as unknown as { __terminalExtraKeys: string[] }
    ).__terminalExtraKeys = log;
    for (const type of ["keydown", "keyup"] as const) {
      input.addEventListener(type, (event) => {
        log.push(`${type}:${event.key}:${event.code}`);
      });
    }
  });
  await page.getByRole("button", { name: "↑", exact: true }).tap();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as { __terminalExtraKeys: string[] })
          .__terminalExtraKeys,
    ),
  ).toEqual(["keydown:ArrowUp:ArrowUp", "keyup:ArrowUp:ArrowUp"]);

  // Reducing the keyboard removes the key line immediately, expires the
  // toggle's intent, and re-arms the suppression.
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 800,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toHaveCount(0);
  await expect(page.getByTitle("Show keyboard")).toBeVisible();
  await expect(input).toHaveAttribute("inputmode", "none");
});

/**
 * A tap on the toggle means "put the keyboard away" only when a keyboard is
 * genuinely up.  When the IME refused the focus transition — iPadOS with the
 * textarea already focused, or a tap landing while the last keyboard was
 * still draining — intent stays lit over a keyboard that never rose, and the
 * next tap is the user asking again, not asking to hide.  Taking the hide
 * branch there is exactly backwards: it made a missed raise cost extra taps.
 */
test("a tap while the keyboard failed to rise retries instead of hiding", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  const input = page.locator('textarea[aria-label="Terminal input"]').first();
  const keyLine = page.getByRole("button", { name: "Esc" });

  // Intent lights, but no keyboard ever rises (no viewport shrink) — the
  // icon keeps offering the keyboard rather than claiming one is up.
  await page.getByTitle("Show keyboard").tap();
  await expect(input).not.toHaveAttribute("inputmode", "none");
  await expect(page.getByTitle("Show keyboard")).toBeVisible();
  await expect(page.getByTitle("Hide keyboard")).toHaveCount(0);

  // The next tap retries the raise: intent stays lit and focus is
  // re-asserted, instead of the old hide branch re-arming the suppression.
  await page.getByTitle("Show keyboard").tap();
  await expect(input).not.toHaveAttribute("inputmode", "none");
  await expect(input).toBeFocused();

  // And the retried intent still tracks a keyboard that does rise.
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toBeVisible();
  await expect(page.getByTitle("Hide keyboard")).toBeVisible();

  // A tap with the keyboard genuinely up still puts it away.
  await page.getByTitle("Hide keyboard").tap();
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 800,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toHaveCount(0);
  await expect(input).toHaveAttribute("inputmode", "none");
});

/**
 * iOS does not merely shrink the visual viewport when the software keyboard
 * rises — it also pans it (offsetTop > 0) to keep the focused input visible.
 * The app answers by pinning <main> with a translateY(offsetTop) transform,
 * and a transformed ancestor becomes the containing block for position:fixed:
 * an overlay rendered inside <main> then added its own band offset on top of
 * <main>'s and landed off-screen — Cmd+K was invisible with the keyboard up.
 * Chrome's metrics override cannot pan the visual viewport, so the test stubs
 * window.visualViewport to reproduce the iOS reading exactly.
 */
test("the switcher stays in view when the keyboard pans the visual viewport", async ({
  page,
}) => {
  await authenticate(page);
  await newTerminal(page);

  // Intent lit and a terminal input holding focus, as with a real keyboard.
  await page.getByTitle("Show keyboard").tap();

  // The iOS reading with a keyboard up: height shrunk by 300, panned by 150.
  // Shadow the getters on the real visualViewport — the app attached its
  // listeners to it at mount, so replacing the object would miss them.
  await page.evaluate(() => {
    const vv = window.visualViewport!;
    Object.defineProperty(vv, "height", { get: () => 500 });
    Object.defineProperty(vv, "offsetTop", { get: () => 150 });
    vv.dispatchEvent(new Event("resize"));
    vv.dispatchEvent(new Event("scroll"));
  });

  // The app believes a keyboard occludes 300px: the key line shows and
  // <main> pins to the band with a translateY transform.
  await expect(page.getByRole("button", { name: "Esc" })).toBeVisible();

  await page.keyboard.press("ControlOrMeta+k");
  const dialog = page.locator('div[role="dialog"]');
  await expect(dialog).toBeVisible({ timeout: 5_000 });

  // The dialog must cover the visible band [150, 650] in page coordinates —
  // not sit under it. Before the portal fix it rendered at [300, 800].
  const box = await dialog.boundingBox();
  expect(box).not.toBeNull();
  expect(box!.y).toBeGreaterThanOrEqual(148);
  expect(box!.y).toBeLessThanOrEqual(152);
  expect(box!.y + box!.height).toBeLessThanOrEqual(652);
});

test("key-line taps cancel the touch so the keyboard stays up", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  const input = page.locator('textarea[aria-label="Terminal input"]').first();
  const keyLine = page.getByRole("button", { name: "Esc" });

  await page.getByTitle("Show keyboard").tap();
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(keyLine).toBeVisible();

  // iPadOS blurs the focused terminal textarea when a tap lands on a
  // non-editable element, and cancelling pointerdown does not stop it — the
  // touchstart itself has to be cancelled or the software keyboard drops on
  // every key-line tap.  Watch for the cancellation at document level, where
  // it is visible once the button's own listener has run.
  await page.evaluate(() => {
    (window as unknown as { __touchCancelled?: boolean }).__touchCancelled =
      undefined;
    document.addEventListener("touchstart", (e) => {
      (window as unknown as { __touchCancelled?: boolean }).__touchCancelled =
        e.defaultPrevented;
    });
  });
  const touchCancelled = () =>
    page.evaluate(
      () =>
        (window as unknown as { __touchCancelled?: boolean }).__touchCancelled,
    );

  for (const name of ["Esc", "Ctrl", "←"]) {
    await page.getByRole("button", { name, exact: true }).tap();
    expect(await touchCancelled()).toBe(true);
  }

  // Focus never left the terminal and the key line is still up.
  await expect(input).toBeFocused();
  await expect(keyLine).toBeVisible();

  // Paste is the one exception: its clipboard read is authorised by a genuine
  // click, which a cancelled touch would suppress.
  await page.getByRole("button", { name: "Paste" }).tap();
  expect(await touchCancelled()).toBe(false);
});

/**
 * Surface panes must not override the icon: a canvas is not editable, so an
 * IME dismisses over it — which used to expire the toggle's intent.  Focus
 * landing on a surface canvas has to reach the surface's hidden IME textarea
 * (which routes keys into the surface) instead, and that textarea carries the
 * same inputmode="none" suppression while the keyboard is not wanted.
 *
 * BlitSurfaceCanvas now performs that handoff itself, on every platform, so
 * that a composition can start at all.  What this test covers is the
 * Workspace-level redirect behind it: a capture-phase net for any canvas in a
 * pane, which is what a synthetic one exercises.  A real surface needs a
 * Wayland client this stack does not run, so the test plants the DOM shape
 * BlitSurfaceCanvas.attach() produces — a tabindex=0 canvas with a labeled
 * textarea beside it — and holds the Workspace policy to it on its own.
 */
test("the icon's keyboard survives focus landing on a surface canvas", async ({
  page,
  context,
}) => {
  await authenticate(page);
  await newTerminal(page);

  await page.evaluate(() => {
    const section = document.querySelector("section");
    if (!section) throw new Error("no pane section");
    const holder = document.createElement("div");
    const ta = document.createElement("textarea");
    ta.setAttribute("aria-label", "Surface input");
    ta.tabIndex = -1;
    const canvas = document.createElement("canvas");
    canvas.tabIndex = 0;
    canvas.dataset.testid = "fake-surface-canvas";
    canvas.style.width = "60px";
    canvas.style.height = "60px";
    holder.append(ta, canvas);
    section.append(holder);
  });
  const surfaceInput = page.locator('textarea[aria-label="Surface input"]');
  const surfaceCanvas = page.getByTestId("fake-surface-canvas");

  // While the keyboard is not wanted, the surface textarea is suppressed
  // exactly like a terminal's (the MutationObserver stamps it on mount),
  // and tapping the surface parks focus on the canvas — hardware keys and
  // pointer input want it there, and nothing must shove an IME up.
  await expect(surfaceInput).toHaveAttribute("inputmode", "none");
  await surfaceCanvas.tap();
  await expect(surfaceCanvas).toBeFocused();

  // Raise the keyboard from the icon and emulate it occluding the viewport.
  await page.getByTitle("Show keyboard").tap();
  await expect(surfaceInput).not.toHaveAttribute("inputmode", "none");
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(page.getByRole("button", { name: "Esc" })).toBeVisible();

  // Tapping the surface focuses its canvas; the redirect must park focus on
  // the IME textarea instead, and the icon's intent must survive.
  await surfaceCanvas.tap();
  await expect(surfaceInput).toBeFocused();
  await expect(page.getByTitle("Hide keyboard")).toBeVisible();

  // The extra row belongs to the keyboard-holding input, not to the last
  // terminal session. Surface panes used to show the row while every key in
  // it was still written to a stale terminal.
  await page.evaluate(() => {
    const input = document.querySelector<HTMLTextAreaElement>(
      'textarea[aria-label="Surface input"]',
    );
    if (!input) throw new Error("no surface input");
    const log: string[] = [];
    (window as unknown as { __surfaceExtraKeys: string[] }).__surfaceExtraKeys =
      log;
    for (const type of ["keydown", "keyup"] as const) {
      input.addEventListener(type, (event) => {
        log.push(`${type}:${event.key}:${event.code}`);
      });
    }
  });
  await page.getByRole("button", { name: "Esc", exact: true }).tap();
  await page.getByRole("button", { name: "/", exact: true }).tap();
  expect(
    await page.evaluate(
      () =>
        (window as unknown as { __surfaceExtraKeys: string[] })
          .__surfaceExtraKeys,
    ),
  ).toEqual([
    "keydown:Escape:Escape",
    "keyup:Escape:Escape",
    "keydown:/:Slash",
    "keyup:/:Slash",
  ]);
});

/**
 * iPadOS only answers a focus CHANGE: focus() on the element that already
 * holds focus is a no-op, and blur+focus within one tap nets to zero — no
 * keyboard.  (The tell on device: switching panes raised the keyboard,
 * because that lands focus on a different element.)  The show path
 * therefore hops focus through a neutral host textarea when the target
 * already holds focus, then hands focus back.  navigator.platform is
 * stubbed to take the iOS branch under Chromium.
 */
test("iOS hops focus through a host when the target already holds it", async ({
  page,
  context,
}) => {
  await page.addInitScript(() => {
    Object.defineProperty(navigator, "platform", { get: () => "iPad" });
  });
  await authenticate(page);
  await newTerminal(page);

  const input = page.locator('textarea[aria-label="Terminal input"]').first();
  // The pane-focus effect has the terminal input holding focus from the
  // start — the case a plain focus() cannot raise a keyboard from.
  await expect(input).toBeFocused();

  await page.getByTitle("Show keyboard").tap();

  // The hop: focus leaves for the neutral host...
  const host = page.locator('textarea[aria-label="Keyboard host"]');
  await expect(host).toBeFocused();

  // ...and the handback is driven by the keyboard actually rising, not a
  // fixed delay: emulate the occlusion now and focus must return to the
  // real target well ahead of the 600ms fallback.
  const cdp = await context.newCDPSession(page);
  await cdp.send("Emulation.setDeviceMetricsOverride", {
    width: 480,
    height: 500,
    deviceScaleFactor: 1,
    mobile: true,
  });
  await expect(input).toBeFocused({ timeout: 550 });
  await expect(input).not.toHaveAttribute("inputmode", "none");
  await expect(page.getByRole("button", { name: "Esc" })).toBeVisible();
});
