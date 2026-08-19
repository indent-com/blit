/**
 * A manage tile's card is its title and nothing else — there is no body to
 * fall back on — so the title has to carry both halves of the address: which
 * server, and which of its panels the pane is on.
 */

import { describe, expect, it } from "vitest";
import { manageAssignment } from "../bsp/layout";
import { tileDisplay } from "../ide/tileDisplay";
import { setShownTab } from "../connectionTab";

describe("tileDisplay: manage", () => {
  it("names the server alone until the panels have resolved a tab", () => {
    const d = tileDisplay(manageAssignment("never-opened"));
    expect(d.kind).toBe("manage");
    expect(d.title).toBe("never-opened:manage");
    // Nothing hides in a second line; the card has one.
    expect(d.subtitle).toBe("");
  });

  it("names the tab the panels are on, by the label the strip uses", () => {
    setShownTab("dev", "session");
    expect(tileDisplay(manageAssignment("dev")).title).toBe(
      "dev:manage > Session",
    );

    // Follows the pane rather than being sampled once: the tile is parked on
    // whichever tab it was left on, and it can be restored and re-parked.
    setShownTab("dev", "systemd");
    expect(tileDisplay(manageAssignment("dev")).title).toBe(
      "dev:manage > systemd",
    );
  });

  it("keeps one connection's tab out of another's card", () => {
    setShownTab("a", "clients");
    setShownTab("b", "extensions");
    expect(tileDisplay(manageAssignment("a")).title).toBe("a:manage > Clients");
    expect(tileDisplay(manageAssignment("b")).title).toBe(
      "b:manage > Extensions",
    );
  });
});
