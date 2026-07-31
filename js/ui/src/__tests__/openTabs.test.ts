import { describe, expect, it } from "vitest";
import { createRoot, createSignal } from "solid-js";
import { createOpenTabs } from "../ide/openTabs";

const encoder = new TextEncoder();

type Live = Map<string, { value: Uint8Array | null; mtimeNs: bigint }>;

const entry = (bare: string, mtimeNs: bigint) => ({
  value: encoder.encode(bare),
  mtimeNs,
});

interface FakeWatch {
  prefix: string;
  live: Live;
  closed: boolean;
  push: () => void;
}

/** A workspace whose watchKv hands back a mirror the test can mutate. */
function fakeWorkspace() {
  const watches = new Map<string, FakeWatch>();
  const workspace = {
    watchKv(
      connectionId: string,
      prefix: string,
      options: { onUpdate?: (m: { live: Live }) => void },
    ) {
      const live: Live = new Map();
      const watch: FakeWatch = {
        prefix,
        live,
        closed: false,
        push: () => options.onUpdate?.({ live }),
      };
      watches.set(connectionId, watch);
      return Promise.resolve({
        kvId: 1,
        mirror: { live },
        close: () => {
          watch.closed = true;
        },
      });
    },
  };
  return { workspace, watches };
}

/** Let the watchKv promise settle and Solid flush the resulting updates. */
const settle = async () => {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
};

const conn = (
  id: string,
  over: { ready?: boolean; supportsKv?: boolean } = {},
) => ({
  id,
  ready: over.ready ?? true,
  supportsKv: over.supportsKv ?? true,
});

describe("createOpenTabs", () => {
  it("merges both hosts' registries, tagging each tab with its connection", async () => {
    const { workspace, watches } = fakeWorkspace();
    const { tabs, dispose } = createRoot((dispose) => ({
      tabs: createOpenTabs(workspace as never, () => [
        conn("box"),
        conn("hub"),
      ]),
      dispose,
    }));
    await settle();

    watches
      .get("box")!
      .live.set("tabs/aaaa1111", entry("editor:/src/a.rs", 20n));
    watches
      .get("hub")!
      .live.set("tabs/bbbb2222", entry("editor:/src/b.rs", 30n));
    watches.get("box")!.push();
    await settle();

    // Sorted newest-registered first, and each bare value re-tagged with the
    // connection whose store it came from.
    expect(tabs().map((t) => t.assignment)).toEqual([
      "editor:hub:/src/b.rs",
      "editor:box:/src/a.rs",
    ]);
    expect(watches.get("box")!.prefix).toBe("tabs/");
    dispose();
  });

  it("ignores connections that are not ready or lack kv", async () => {
    const { workspace, watches } = fakeWorkspace();
    const { tabs, dispose } = createRoot((dispose) => ({
      tabs: createOpenTabs(workspace as never, () => [
        conn("cold", { ready: false }),
        conn("old", { supportsKv: false }),
      ]),
      dispose,
    }));
    await settle();

    expect(watches.size).toBe(0);
    expect(tabs()).toEqual([]);
    dispose();
  });

  it("drops a tab when the registry record is deleted", async () => {
    const { workspace, watches } = fakeWorkspace();
    const { tabs, dispose } = createRoot((dispose) => ({
      tabs: createOpenTabs(workspace as never, () => [conn("box")]),
      dispose,
    }));
    await settle();

    const watch = watches.get("box")!;
    watch.live.set("tabs/aaaa1111", entry("editor:/src/a.rs", 10n));
    watch.push();
    await settle();
    expect(tabs()).toHaveLength(1);

    watch.live.delete("tabs/aaaa1111");
    watch.push();
    await settle();
    expect(tabs()).toEqual([]);
    dispose();
  });

  it("skips malformed and metadata-only records", async () => {
    const { workspace, watches } = fakeWorkspace();
    const { tabs, dispose } = createRoot((dispose) => ({
      tabs: createOpenTabs(workspace as never, () => [conn("box")]),
      dispose,
    }));
    await settle();

    const watch = watches.get("box")!;
    // Not an assignment we know how to re-tag.
    watch.live.set("tabs/aaaa1111", entry("nonsense", 10n));
    // Value too large to have arrived inline — nothing to decode.
    watch.live.set("tabs/bbbb2222", { value: null, mtimeNs: 11n });
    watch.live.set("tabs/cccc3333", entry("editor:/src/a.rs", 12n));
    watch.push();
    await settle();

    expect(tabs().map((t) => t.assignment)).toEqual(["editor:box:/src/a.rs"]);
    dispose();
  });

  it("closes the watch when its connection goes away", async () => {
    const { workspace, watches } = fakeWorkspace();
    const [conns, setConns] = createSignal([conn("box")]);
    const { tabs, dispose } = createRoot((dispose) => ({
      tabs: createOpenTabs(workspace as never, conns),
      dispose,
    }));
    await settle();

    watches
      .get("box")!
      .live.set("tabs/aaaa1111", entry("editor:/src/a.rs", 10n));
    watches.get("box")!.push();
    await settle();
    expect(tabs()).toHaveLength(1);

    setConns([]);
    await settle();
    expect(watches.get("box")!.closed).toBe(true);
    expect(tabs()).toEqual([]);
    dispose();
  });

  it("closes every watch on dispose", async () => {
    const { workspace, watches } = fakeWorkspace();
    const dispose = createRoot((d) => {
      createOpenTabs(workspace as never, () => [conn("box"), conn("hub")]);
      return d;
    });
    await settle();
    expect(watches.size).toBe(2);

    dispose();
    expect([...watches.values()].every((w) => w.closed)).toBe(true);
  });
});
