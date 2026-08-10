import { describe, expect, it, vi } from "vitest";
import { BlitActivityStore } from "../activity";

describe("BlitActivityStore", () => {
  it("publishes immutable progress snapshots and removes finished work", () => {
    const store = new BlitActivityStore();
    const listener = vi.fn();
    store.subscribe(listener);

    const upload = store.begin({
      kind: "upload",
      label: "shot.png",
      target: "Slack",
      completed: 0,
      total: 100,
    });
    const first = store.getSnapshot();
    expect(first).toHaveLength(1);
    expect(first[0]).toEqual(
      expect.objectContaining({
        id: upload.id,
        label: "shot.png",
        completed: 0,
        total: 100,
      }),
    );

    upload.update({ completed: 40 });
    expect(store.getSnapshot()).not.toBe(first);
    expect(store.getSnapshot()[0].completed).toBe(40);
    expect(first[0].completed).toBe(0);

    upload.finish();
    upload.finish();
    expect(store.getSnapshot()).toEqual([]);
    expect(listener).toHaveBeenCalledTimes(3);
  });

  it("keeps concurrent activities in start order", () => {
    const store = new BlitActivityStore();
    const first = store.begin({ kind: "search", label: "src" });
    const second = store.begin({ kind: "sync", label: "/work" });

    expect(store.getSnapshot().map((activity) => activity.id)).toEqual([
      first.id,
      second.id,
    ]);
    first.finish();
    expect(store.getSnapshot().map((activity) => activity.id)).toEqual([
      second.id,
    ]);
  });
});
