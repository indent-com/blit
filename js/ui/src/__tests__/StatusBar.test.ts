import { describe, expect, it } from "vitest";
import type { BlitActivity } from "@blit-sh/core";
import { activityDescription, activityPercent } from "../activityStatus";

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
