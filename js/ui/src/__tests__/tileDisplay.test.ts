/**
 * A manage tile's card is its title and nothing else — there is no body to
 * fall back on — so the title carries both halves, in the two fields a
 * terminal card splits `dev:1 › zsh` into: the address, drawn dim, and the
 * name, which for these panels is the tab they are on.
 */

import { describe, expect, it } from "vitest";
import { manageAssignment } from "../bsp/layout";
import { tileDisplay } from "../ide/tileDisplay";
import { setShownTab } from "../connectionTab";

describe("tileDisplay: manage", () => {
  it("is the address alone until the panels have resolved a tab", () => {
    const d = tileDisplay(manageAssignment("never-opened"));
    expect(d.kind).toBe("manage");
    expect(d.prefix).toBe("never-opened:manage");
    // Empty rather than a guess — and the card draws no separator for it.
    expect(d.title).toBe("");
    // Nothing hides in a second line; the card has one.
    expect(d.subtitle).toBe("");
  });

  it("names the tab the panels are on, by the label the strip uses", () => {
    setShownTab("dev", "session");
    expect(tileDisplay(manageAssignment("dev"))).toMatchObject({
      prefix: "dev:manage",
      title: "Session",
    });

    // Follows the pane rather than being sampled once: the tile is parked on
    // whichever tab it was left on, and it can be restored and re-parked.
    setShownTab("dev", "systemd");
    expect(tileDisplay(manageAssignment("dev")).title).toBe("systemd");
  });

  it("keeps one connection's tab out of another's card", () => {
    setShownTab("a", "clients");
    setShownTab("b", "extensions");
    expect(tileDisplay(manageAssignment("a"))).toMatchObject({
      prefix: "a:manage",
      title: "Clients",
    });
    expect(tileDisplay(manageAssignment("b"))).toMatchObject({
      prefix: "b:manage",
      title: "Extensions",
    });
  });

  it("leaves every other kind without an address half", () => {
    // The prefix is the manage card's whole reason to exist; an editor names
    // itself by its file and would only repeat its path in a dimmer font.
    expect(tileDisplay("editor:dev:/src/blit/README.md")).toMatchObject({
      prefix: "",
      title: "README.md",
      subtitle: "/src/blit/README.md",
    });
  });
});
