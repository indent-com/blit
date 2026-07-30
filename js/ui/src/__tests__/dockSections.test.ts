import { describe, expect, it } from "vitest";
import type { LeftPanel } from "../LeftDock";
import { foldedSections, liveOverrides, toggleSection } from "../dockSections";

const set = (...panels: LeftPanel[]) => new Set<LeftPanel>(panels);
const none = new Set<LeftPanel>();

describe("foldedSections", () => {
  it("folds what does not apply on top of what the user collapsed", () => {
    expect(foldedSections(set("problems"), set("log"), none)).toEqual(
      set("problems", "log"),
    );
  });

  it("leaves an overridden section open", () => {
    // The user opened the auto-folded log to read why it is empty.
    expect(foldedSections(none, set("log"), set("log"))).toEqual(none);
  });

  it("keeps a user collapse even when the section is overridden", () => {
    // The override only answers the auto-fold; a deliberate collapse stands.
    expect(foldedSections(set("log"), set("log"), set("log"))).toEqual(
      set("log"),
    );
  });
});

describe("toggleSection", () => {
  it("records a collapse for a section that applies", () => {
    const next = toggleSection("log", none, none, none);
    expect(next.userCollapsed).toEqual(set("log"));
    expect(next.overridden).toEqual(none);
  });

  it("opens an auto-folded section without touching the preference", () => {
    // Otherwise the click would store a collapse the section already looks
    // like it has, and the header would stay shut.
    const next = toggleSection("log", none, set("log"), none);
    expect(next.userCollapsed).toEqual(none);
    expect(next.overridden).toEqual(set("log"));
    expect(foldedSections(none, set("log"), next.overridden)).toEqual(none);
  });

  it("re-folds an overridden section on the next click", () => {
    const next = toggleSection("log", none, set("log"), set("log"));
    expect(next.overridden).toEqual(none);
    expect(next.userCollapsed).toEqual(none);
  });

  it("un-collapses a user-collapsed section that also does not apply", () => {
    // The preference is what is showing, so the click answers that first;
    // the auto-fold then keeps it shut until it is clicked again.
    const next = toggleSection("log", set("log"), set("log"), none);
    expect(next.userCollapsed).toEqual(none);
    expect(next.overridden).toEqual(none);
    expect(
      foldedSections(next.userCollapsed, set("log"), next.overridden),
    ).toEqual(set("log"));
  });
});

describe("liveOverrides", () => {
  it("drops overrides once the section applies again", () => {
    // Moving to a root that has a repository: the log must come back on its
    // own, and the next root without one must fold it afresh.
    expect(liveOverrides(set("log"), none)).toEqual(none);
    expect(liveOverrides(set("log"), set("log"))).toEqual(set("log"));
  });
});
