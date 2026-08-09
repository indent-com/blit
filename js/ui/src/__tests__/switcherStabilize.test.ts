import { describe, expect, it } from "vitest";
import { stabilizeSections, type KeyedSection } from "../switcherStabilize";

interface TestItem {
  key: string;
  title: string;
  subtitle: string;
}

function item(key: string, title = key, subtitle = ""): TestItem {
  return { key, title, subtitle };
}

describe("stabilizeSections", () => {
  it("returns next as-is when there is no previous value", () => {
    const next: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a")] },
    ];
    expect(stabilizeSections(undefined, next)).toBe(next);
  });

  it("reuses section and item references when content is identical", () => {
    const prev: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a", "shell", "bash"), item("b")] },
      { title: "Surfaces", items: [item("c")] },
    ];
    const rebuilt: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a", "shell", "bash"), item("b")] },
      { title: "Surfaces", items: [item("c")] },
    ];
    const out = stabilizeSections(prev, rebuilt);
    expect(out[0]).toBe(prev[0]);
    expect(out[1]).toBe(prev[1]);
    expect(out[0].items[0]).toBe(prev[0].items[0]);
  });

  it("rebuilds only the changed item and its section", () => {
    const prev: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a"), item("b", "old")] },
      { title: "Surfaces", items: [item("c")] },
    ];
    const next: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a"), item("b", "new")] },
      { title: "Surfaces", items: [item("c")] },
    ];
    const out = stabilizeSections(prev, next);
    expect(out[0]).not.toBe(prev[0]);
    expect(out[0].items[0]).toBe(prev[0].items[0]);
    expect(out[0].items[1]).toBe(next[0].items[1]);
    expect(out[1]).toBe(prev[1]);
  });

  it("reuses item objects across a reorder, in a new section object", () => {
    const prev: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a"), item("b")] },
    ];
    const next: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("b"), item("a")] },
    ];
    const out = stabilizeSections(prev, next);
    expect(out[0]).not.toBe(prev[0]);
    expect(out[0].items[0]).toBe(prev[0].items[1]);
    expect(out[0].items[1]).toBe(prev[0].items[0]);
  });

  it("rebuilds on added, removed, and retitled sections", () => {
    const prev: KeyedSection<TestItem>[] = [
      { title: "Terminals", items: [item("a"), item("b")] },
    ];
    const added = stabilizeSections(prev, [
      { title: "Terminals", items: [item("a"), item("b"), item("c")] },
    ]);
    expect(added[0]).not.toBe(prev[0]);
    expect(added[0].items[0]).toBe(prev[0].items[0]);

    const removed = stabilizeSections(prev, [
      { title: "Terminals", items: [item("a")] },
    ]);
    expect(removed[0]).not.toBe(prev[0]);
    expect(removed[0].items[0]).toBe(prev[0].items[0]);

    const retitled = stabilizeSections(prev, [
      { title: "Other", items: [item("a"), item("b")] },
    ]);
    expect(retitled[0]).not.toBe(prev[0]);
  });

  it("treats differing field counts as different content", () => {
    const prev: KeyedSection<TestItem>[] = [{ title: "s", items: [item("a")] }];
    const withExtra = [
      {
        title: "s",
        items: [{ key: "a", title: "a", subtitle: "", extra: 1 }],
      },
    ];
    const out = stabilizeSections(
      prev,
      withExtra as unknown as KeyedSection<TestItem>[],
    );
    expect(out[0].items[0]).not.toBe(prev[0].items[0]);
  });
});
