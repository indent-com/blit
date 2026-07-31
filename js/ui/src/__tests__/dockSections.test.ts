import { afterEach, describe, expect, it } from "vitest";
import type { LeftPanel } from "../LeftDock";
import { foldedSections, liveOverrides, toggleSection } from "../dockSections";
import { preferredCollapsedSections } from "../storage";

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

/**
 * Entering a repository has to open the commit log, and leaving one has to
 * close it. Both fall out of `noRepo` driving the fold — but only as long as
 * nothing seeds a *user collapse* for the log, because a preference outranks
 * the auto-unfold by design. The first-run default used to do exactly that,
 * which left the log shut on entering a repo for good.
 */
describe("commit log follows the repository", () => {
  afterEach(() => localStorage.clear());

  const firstRun = () => new Set(preferredCollapsedSections() as LeftPanel[]);

  it("does not pre-collapse the log on first run", () => {
    expect(firstRun().has("log")).toBe(false);
  });

  it("folds outside a repository and opens inside one", () => {
    const outside = foldedSections(firstRun(), set("log"), none);
    expect(outside.has("log")).toBe(true);
    const inside = foldedSections(firstRun(), none, none);
    expect(inside.has("log")).toBe(false);
  });

  it("still honours an explicit collapse made inside a repository", () => {
    // Collapsing an applicable section records a preference...
    const { userCollapsed } = toggleSection("log", firstRun(), none, none);
    expect(userCollapsed.has("log")).toBe(true);
    // ...which survives the next repo, unlike an auto-fold.
    expect(foldedSections(userCollapsed, none, none).has("log")).toBe(true);
  });

  it("keeps Problems folded on first run", () => {
    // It has an auto-fold of its own, but no LSP is the common case and the
    // panel is noise before one attaches.
    expect(firstRun().has("problems")).toBe(true);
  });
});
