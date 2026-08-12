import { describe, expect, it } from "vitest";
import type { BlitActivity } from "@blit-sh/core";
import { activityDescription, activityPercent } from "../activityStatus";
import { FIT_HYSTERESIS_PX, nextCompact } from "../statusBarFit";

function activity(update: Partial<BlitActivity> = {}): BlitActivity {
  return {
    id: 1,
    kind: "upload",
    label: "shot.png",
    target: "Slack",
    completed: 25,
    total: 100,
    startedAt: 1,
    ...update,
  };
}

describe("status-bar activities", () => {
  it("formats upload identity and determinate progress", () => {
    const upload = activity();
    expect(activityDescription(upload)).toBe("Uploading shot.png › Slack");
    expect(activityPercent(upload)).toBe(25);
    expect(activityPercent(activity({ completed: 150 }))).toBe(100);
  });

  it("leaves operations without a total indeterminate", () => {
    const sync = activity({
      kind: "sync",
      label: "/work",
      target: undefined,
      completed: undefined,
      total: undefined,
    });
    expect(activityDescription(sync)).toBe("Syncing /work");
    expect(activityPercent(sync)).toBeNull();
  });
});

describe("status-bar icon collapse", () => {
  const min = 156;
  const expanded = {
    identity: 400,
    icons: 120,
    expandedIcons: 120,
    minIdentity: min,
  };
  const collapsed = {
    identity: 130,
    icons: 24,
    expandedIcons: 120,
    minIdentity: min,
  };

  it("keeps the icons while the title has room", () => {
    expect(nextCompact(false, expanded)).toBe(false);
    expect(nextCompact(false, { ...expanded, identity: min })).toBe(false);
  });

  it("collapses once the title is squeezed under the floor", () => {
    expect(nextCompact(false, { ...expanded, identity: min - 1 })).toBe(true);
  });

  it("stays collapsed until unfolding would still clear the floor", () => {
    // Unfolding costs 96px here (120 icons - 24 menu button).
    const onTheEdge = { ...collapsed, identity: min + 96 };
    expect(nextCompact(true, onTheEdge)).toBe(true);
    expect(nextCompact(true, collapsed)).toBe(true);
    expect(
      nextCompact(true, {
        ...collapsed,
        identity: min + 96 + FIT_HYSTERESIS_PX,
      }),
    ).toBe(false);
  });

  it("stays collapsed when the expanded width was never measured", () => {
    expect(
      nextCompact(true, { ...collapsed, expandedIcons: null, identity: 9999 }),
    ).toBe(true);
  });
});
