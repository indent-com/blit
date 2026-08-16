import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { createLease } from "../ide/lease";

/** The leases behind the dock's lazy IDE resources: a folded section unmounts
 *  its panel, and the resource (a directory watch, a commit-log walk, a
 *  language server) has to go with it — but only once *nothing* wants it. */
describe("ide resource lease", () => {
  it("is unwanted until someone asks, and again once everyone lets go", () => {
    createRoot((dispose) => {
      const { wanted, acquire } = createLease();
      expect(wanted()).toBe(false);
      const release = acquire();
      expect(wanted()).toBe(true);
      release();
      expect(wanted()).toBe(false);
      dispose();
    });
  });

  it("keeps the resource while a second consumer still holds a lease", () => {
    createRoot((dispose) => {
      const { wanted, acquire } = createLease();
      // The Problems panel and the symbol switcher both want the language
      // server; whichever closes first must not close it under the other.
      const releasePanel = acquire();
      const releaseSwitcher = acquire();
      releasePanel();
      expect(wanted()).toBe(true);
      releaseSwitcher();
      expect(wanted()).toBe(false);
      dispose();
    });
  });

  it("ignores a repeated release rather than underflowing the count", () => {
    createRoot((dispose) => {
      const { wanted, acquire } = createLease();
      const releaseA = acquire();
      releaseA();
      releaseA(); // a double release must not go negative
      const releaseB = acquire();
      // Without idempotence the count would now be 0 and this would read
      // false — stranding a live watch nothing can ever close.
      expect(wanted()).toBe(true);
      releaseB();
      expect(wanted()).toBe(false);
      dispose();
    });
  });

  it("re-acquires after going idle, so reopening a panel reattaches", () => {
    createRoot((dispose) => {
      const { wanted, acquire } = createLease();
      acquire()();
      expect(wanted()).toBe(false);
      const release = acquire();
      expect(wanted()).toBe(true);
      release();
      dispose();
    });
  });
});
