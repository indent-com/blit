import { describe, expect, it, vi } from "vitest";
import {
  EVENT_CAP,
  followMuster,
  groupUnits,
  MusterMirror,
  openMuster,
  unitCanStop,
  unitStartVerb,
  type MusterUnit,
} from "../muster";

function unit(name: string, over: Record<string, unknown> = {}) {
  return {
    name,
    instance: null,
    description: `${name} unit`,
    phase: "running",
    pty: 7,
    restarts: 0,
    lastExit: null,
    requires: [],
    autostart: true,
    stale: false,
    type: "simple",
    surfaces: [],
    runs: [],
    ...over,
  };
}

describe("MusterMirror", () => {
  it("takes the directory from the greeting and stays unready", () => {
    const mirror = new MusterMirror();
    mirror.apply(
      JSON.stringify({ type: "hello", version: 1, dir: "/home/p/.config/m" }),
    );
    expect(mirror.dir).toBe("/home/p/.config/m");
    // The greeting says where, not what: until a full frame lands, an empty
    // table means "not told yet", which is not the same as "no units".
    expect(mirror.ready).toBe(false);
  });

  it("replaces the table on a full frame and drops what it omits", () => {
    const mirror = new MusterMirror();
    mirror.apply(
      JSON.stringify({
        type: "state",
        full: true,
        dir: "/d",
        units: [unit("api"), unit("web")],
        instances: [{ name: "main", stack: "dev", members: ["main/api"] }],
        gone: [],
      }),
    );
    expect([...mirror.units.keys()]).toEqual(["api", "web"]);
    expect(mirror.instances.get("main")?.stack).toBe("dev");
    expect(mirror.ready).toBe(true);

    mirror.apply(
      JSON.stringify({
        type: "state",
        full: true,
        units: [unit("api")],
        instances: [],
        gone: [],
      }),
    );
    // Not listed in a full frame is gone; a full frame is the whole truth.
    expect([...mirror.units.keys()]).toEqual(["api"]);
    expect(mirror.instances.size).toBe(0);
  });

  it("merges a partial frame and honours its gone list", () => {
    const mirror = new MusterMirror();
    mirror.apply(
      JSON.stringify({
        type: "state",
        full: true,
        units: [unit("api"), unit("web")],
        instances: [],
        gone: [],
      }),
    );
    mirror.apply(
      JSON.stringify({
        type: "state",
        units: [unit("api", { phase: "backoff", restarts: 3 })],
        gone: ["web"],
      }),
    );
    expect(mirror.units.get("api")?.phase).toBe("backoff");
    expect(mirror.units.get("api")?.restarts).toBe(3);
    expect(mirror.units.has("web")).toBe(false);
    // A partial frame names only what changed, so everything else it does not
    // mention is untouched rather than absent.
    expect(mirror.units.size).toBe(1);
  });

  it("keeps a unit whole rather than patching its fields", () => {
    const mirror = new MusterMirror();
    mirror.apply(
      JSON.stringify({
        type: "state",
        full: true,
        units: [
          unit("web", {
            surfaces: [{ id: 4, title: "x", width: 1, height: 2 }],
          }),
        ],
        instances: [],
      }),
    );
    mirror.apply(
      JSON.stringify({ type: "state", units: [unit("web")], gone: [] }),
    );
    // The replacement carries no surfaces, so the unit has none — a frame is
    // not a patch, and remembering the old list would show a dead window.
    expect(mirror.units.get("web")?.surfaces).toEqual([]);
  });

  it("appends event batches, caps them, and reports each batch", () => {
    const seen: string[] = [];
    const mirror = new MusterMirror((events) => {
      for (const event of events) seen.push(event.event);
    });
    mirror.apply(
      JSON.stringify({
        type: "events",
        records: [
          { seq: 1, ts: 10, unit: "api", event: "started", phase: "running" },
          { seq: 2, ts: 11, unit: "api", event: "exited", phase: "backoff" },
        ],
      }),
    );
    expect(seen).toEqual(["started", "exited"]);
    expect(mirror.events.map((e) => e.seq)).toEqual([1, 2]);

    mirror.apply(
      JSON.stringify({
        type: "events",
        records: Array.from({ length: EVENT_CAP + 10 }, (_, index) => ({
          seq: index + 3,
          ts: 12,
          unit: "api",
          event: "tick",
          phase: "running",
        })),
      }),
    );
    expect(mirror.events.length).toBe(EVENT_CAP);
    // The cap drops from the front: what is kept is the newest.
    expect(mirror.events[mirror.events.length - 1]?.seq).toBe(EVENT_CAP + 12);
  });

  it("ignores malformed payloads and unknown message types", () => {
    const mirror = new MusterMirror();
    expect(() => mirror.apply("not json")).not.toThrow();
    expect(() =>
      mirror.apply(JSON.stringify({ type: "future" })),
    ).not.toThrow();
    expect(() => mirror.apply(JSON.stringify([1, 2]))).not.toThrow();
    expect(mirror.units.size).toBe(0);
  });

  it("drops rows it cannot identify rather than inventing a name", () => {
    const mirror = new MusterMirror();
    mirror.apply(
      JSON.stringify({
        type: "state",
        full: true,
        units: [unit("api"), { description: "nameless" }, 7],
        instances: [{ stack: "dev" }],
      }),
    );
    expect([...mirror.units.keys()]).toEqual(["api"]);
    expect(mirror.instances.size).toBe(0);
  });
});

describe("groupUnits", () => {
  const build = (rows: MusterUnit[]) =>
    new Map(rows.map((row) => [row.name, row]));

  it("nests members under their instance and lists the rest after", () => {
    const units = build([
      unit("main/api") as unknown as MusterUnit,
      unit("main/web") as unknown as MusterUnit,
      unit("standalone") as unknown as MusterUnit,
    ]);
    const instances = new Map([
      [
        "main",
        { name: "main", stack: "dev", members: ["main/api", "main/web"] },
      ],
    ]);
    const groups = groupUnits(units, instances);
    expect(groups.map((g) => g.instance?.name ?? null)).toEqual(["main", null]);
    expect(groups[0]?.units.map((u) => u.name)).toEqual([
      "main/api",
      "main/web",
    ]);
    expect(groups[1]?.units.map((u) => u.name)).toEqual(["standalone"]);
  });

  it("keeps an instance whose expansion produced nothing", () => {
    const groups = groupUnits(
      new Map(),
      new Map([["broken", { name: "broken", stack: "dev", members: [] }]]),
    );
    // A stack that failed to expand is declared but empty; dropping the group
    // would make a broken instance look like one that was never written.
    expect(groups).toEqual([
      { instance: { name: "broken", stack: "dev", members: [] }, units: [] },
    ]);
  });

  it("omits the loose group when every unit belongs to an instance", () => {
    const units = build([unit("main/api") as unknown as MusterUnit]);
    const instances = new Map([
      ["main", { name: "main", stack: "dev", members: ["main/api"] }],
    ]);
    expect(groupUnits(units, instances).length).toBe(1);
  });
});

describe("unitStartVerb", () => {
  it("restarts completed oneshots instead of sending a no-op start", () => {
    expect(unitStartVerb({ phase: "exited" })).toBe("restart");
  });

  it("restarts live units and starts inactive units", () => {
    expect(unitStartVerb({ phase: "running" })).toBe("restart");
    expect(unitStartVerb({ phase: "activating" })).toBe("restart");
    expect(unitStartVerb({ phase: "stopped" })).toBe("start");
    expect(unitStartVerb({ phase: "failed" })).toBe("start");
  });
});

describe("unitCanStop", () => {
  it("hides Stop for a completed oneshot", () => {
    expect(unitCanStop({ phase: "exited", type: "oneshot" })).toBe(false);
  });

  it("keeps Stop for live oneshots and ordinary units", () => {
    expect(unitCanStop({ phase: "activating", type: "oneshot" })).toBe(true);
    expect(unitCanStop({ phase: "running", type: "simple" })).toBe(true);
    expect(unitCanStop({ phase: "exited", type: "simple" })).toBe(true);
  });
});

describe("openMuster", () => {
  function fakeChannel() {
    const sent: string[] = [];
    let onData: ((payload: Uint8Array) => void) | undefined;
    const connection = {
      connectChannel: vi.fn(async (_name: string, options?: any) => {
        onData = options?.onData;
        return {
          channelId: 1,
          name: "blit.muster.v1",
          peer: "ext:1:0",
          metadata: new Uint8Array(),
          availableCredit: 1_000_000n,
          send: (payload: Uint8Array | string) => {
            sent.push(
              typeof payload === "string"
                ? payload
                : new TextDecoder().decode(payload),
            );
            return true;
          },
          close: () => {},
        };
      }),
    };
    return {
      connection,
      sent,
      push: (value: unknown) =>
        onData?.(new TextEncoder().encode(JSON.stringify(value))),
    };
  }

  it("sends the CLI's verbs as bare lines", async () => {
    const { connection, sent } = fakeChannel();
    const handle = await openMuster(connection);
    handle.start("main");
    handle.stop("main/api");
    handle.restart("web");
    handle.rewatch();
    handle.resync();
    expect(sent).toEqual([
      "start main",
      "stop main/api",
      "restart web",
      "rewatch",
      "resync",
    ]);
  });

  it("mirrors what arrives on the channel", async () => {
    const { connection, push } = fakeChannel();
    const handle = await openMuster(connection);
    expect(handle.ready).toBe(false);
    push({ type: "hello", version: 1, dir: "/d" });
    push({ type: "state", full: true, units: [unit("api")], instances: [] });
    expect(handle.dir).toBe("/d");
    expect(handle.ready).toBe(true);
    expect(handle.units.get("api")?.pty).toBe(7);
  });
});

describe("followMuster", () => {
  it("opens a fresh handle after the supervisor channel closes", async () => {
    vi.useFakeTimers();
    try {
      const closures: Array<() => void> = [];
      const connection = {
        connectChannel: vi.fn(async (_name: string, options?: any) => {
          closures.push(() => options?.onClosed?.(0, "replaced"));
          return {
            channelId: closures.length,
            name: "blit.muster.v1",
            peer: "ext:1:0",
            metadata: new Uint8Array(),
            availableCredit: 1_000_000n,
            send: () => true,
            close: () => {},
          };
        }),
      };
      const handles: Array<"open" | "closed"> = [];
      const stop = followMuster(() => connection, {
        onHandle: (handle) => handles.push(handle ? "open" : "closed"),
        retryDelayMs: 10,
      });

      await vi.advanceTimersByTimeAsync(0);
      expect(connection.connectChannel).toHaveBeenCalledTimes(1);
      expect(handles).toEqual(["open"]);

      closures[0]?.();
      expect(handles).toEqual(["open", "closed"]);
      await vi.advanceTimersByTimeAsync(10);
      expect(connection.connectChannel).toHaveBeenCalledTimes(2);
      expect(handles).toEqual(["open", "closed", "open"]);
      stop();
    } finally {
      vi.useRealTimers();
    }
  });
});
