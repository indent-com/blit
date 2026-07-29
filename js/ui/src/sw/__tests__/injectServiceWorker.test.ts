import { describe, expect, it } from "vitest";
import type { PreviewTarget } from "@blit-sh/core";
import { shimTag } from "../inject";

const target: PreviewTarget = {
  dest: "local",
  scheme: "http",
  host: "localhost",
  port: 7777,
};

/** Run the shim the way a previewed page would, in this jsdom window. */
function runShim(cookie = ""): void {
  const html = new TextDecoder().decode(shimTag(target, cookie));
  const source = html.slice("<script>".length, -"</script>".length);
  new Function(source)();
}

describe("service worker shim", () => {
  // Hiding the API from the page must not cut the *shims* off from the
  // worker. It did once: only the cookie shim was moved to the captured
  // handle, so the WebSocket shim read `navigator.serviceWorker` after it had
  // been deleted, found nothing, and failed every relayed socket — which is
  // every dev server's HMR client, so previewed apps stopped loading.
  it("keeps its own handle on the worker after hiding the API", () => {
    const posted: unknown[] = [];
    const controller = { postMessage: (m: unknown) => posted.push(m) };
    Object.defineProperty(Navigator.prototype, "serviceWorker", {
      configurable: true,
      get: () => ({ controller }),
    });

    runShim("a=1");
    expect("serviceWorker" in navigator).toBe(false);

    // The WebSocket shim must still find the worker. A same-host ws:// URL is
    // the case it relays.
    const ws = new WebSocket(`ws://${window.location.host}/hmr`);
    expect(ws).toBeTruthy();
    expect(
      posted.some(
        (m) => (m as { type?: string } | null)?.type === "blit-ws-open",
      ),
      "the relayed socket must reach the worker",
    ).toBe(true);

    // And so must the cookie shim.
    posted.length = 0;
    document.cookie = "b=2";
    expect(
      posted.some(
        (m) => (m as { type?: string } | null)?.type === "blit-cookie",
      ),
      "a cookie write must reach the worker",
    ).toBe(true);
  });

  // A previewed app registering its own worker reaches for blit's origin, not
  // its dev server: the script fetch bypasses the controlling worker by spec.
  // Against a dev server that leaves /sw.js to a SPA fallback the browser
  // refuses it as text/html on every load; against blit proper it would
  // register blit's *own* preview worker at scope "/".
  it("makes the frame report no service-worker support", () => {
    // jsdom has no serviceWorker to begin with, so give it one to remove.
    Object.defineProperty(Navigator.prototype, "serviceWorker", {
      configurable: true,
      get: () => ({ controller: null, register: () => Promise.resolve() }),
    });
    expect("serviceWorker" in navigator).toBe(true);

    runShim();

    // The guard every well-behaved app uses now answers false, so it skips
    // registration rather than failing at it.
    expect("serviceWorker" in navigator).toBe(false);
  });

  it("refuses registration when the accessor cannot be removed", async () => {
    Object.defineProperty(Navigator.prototype, "serviceWorker", {
      configurable: false,
      get: () => ({ controller: null, register: () => Promise.resolve() }),
    });

    runShim();

    // Still present — so the fallback must be a register() that rejects,
    // which is the path apps already have for unsupported browsers.
    expect("serviceWorker" in navigator).toBe(true);
    await expect(
      (
        navigator.serviceWorker as unknown as { register: () => Promise<void> }
      ).register(),
    ).rejects.toThrow(/cannot own a service worker/);
    expect(navigator.serviceWorker.controller).toBeNull();
    await expect(navigator.serviceWorker.getRegistrations()).resolves.toEqual(
      [],
    );
  });
});
