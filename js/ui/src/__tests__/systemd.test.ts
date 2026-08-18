import { describe, expect, it, vi } from "vitest";
import {
  filterUnits,
  openSystemdUnits,
  SystemdUnitsMirror,
  unitStates,
  type SystemdChange,
} from "../systemd";

function unit(name: string, active: string, sub: string) {
  return { name, load: "loaded", active, sub, description: `${name} unit` };
}

describe("SystemdUnitsMirror", () => {
  it("folds a chunked snapshot into one scope map", () => {
    const mirror = new SystemdUnitsMirror();
    mirror.apply(
      JSON.stringify({
        type: "hello",
        ts: 1,
        scopes: [{ scope: "system", source: "gdbus", units: 2 }],
      }),
    );
    expect(mirror.scopes.get("system")?.source).toBe("gdbus");
    expect(mirror.scopes.get("system")?.ready).toBe(false);

    mirror.apply(
      JSON.stringify({
        type: "snapshot",
        scope: "system",
        ts: 2,
        chunk: 0,
        units: [unit("a.service", "active", "running")],
        last: false,
      }),
    );
    // A snapshot is only visible once its last chunk lands: half a unit table
    // is worse than the previous one.
    expect(mirror.scopes.get("system")?.units.size).toBe(0);

    mirror.apply(
      JSON.stringify({
        type: "snapshot",
        scope: "system",
        ts: 3,
        chunk: 1,
        units: [unit("b.service", "inactive", "dead")],
        last: true,
      }),
    );
    const scope = mirror.scopes.get("system")!;
    expect(scope.ready).toBe(true);
    expect([...scope.units.keys()]).toEqual(["a.service", "b.service"]);
    expect(scope.updatedAt).toBe(3);
  });

  it("ignores a snapshot chunk whose head was lost", () => {
    const mirror = new SystemdUnitsMirror();
    mirror.apply(
      JSON.stringify({
        type: "snapshot",
        scope: "system",
        chunk: 3,
        units: [unit("stray.service", "active", "running")],
        last: true,
      }),
    );
    expect(mirror.scopes.get("system")?.units.size).toBe(0);
    expect(mirror.scopes.get("system")?.ready).toBe(false);
  });

  it("applies deltas and reports them once", () => {
    const changes: SystemdChange[] = [];
    const mirror = new SystemdUnitsMirror((change) => changes.push(change));
    mirror.apply(
      JSON.stringify({
        type: "snapshot",
        scope: "user",
        chunk: 0,
        units: [unit("a.service", "active", "running")],
        last: true,
      }),
    );
    const listener = vi.fn();
    mirror.subscribe(listener);

    mirror.apply(
      JSON.stringify({
        type: "change",
        scope: "user",
        ts: 9,
        added: [unit("new.service", "activating", "start")],
        changed: [
          {
            ...unit("a.service", "failed", "failed"),
            previous: { load: "loaded", active: "active", sub: "running" },
          },
        ],
        removed: ["gone.service"],
      }),
    );

    const scope = mirror.scopes.get("user")!;
    expect(scope.units.get("a.service")?.active).toBe("failed");
    expect(scope.units.get("new.service")?.sub).toBe("start");
    expect(listener).toHaveBeenCalledTimes(1);
    expect(changes).toHaveLength(1);
    expect(changes[0]!.changed[0]!.previous.active).toBe("active");
    expect(changes[0]!.removed).toEqual(["gone.service"]);
  });

  it("keeps scopes independent and answers cross-scope lookups", () => {
    const mirror = new SystemdUnitsMirror();
    for (const scope of ["system", "user"]) {
      mirror.apply(
        JSON.stringify({
          type: "snapshot",
          scope,
          chunk: 0,
          units: [unit(`${scope}.service`, "active", "running")],
          last: true,
        }),
      );
    }
    expect(mirror.unit("user.service")?.name).toBe("user.service");
    expect(mirror.unit("user.service", "system")).toBeUndefined();
    expect(mirror.all().map((row) => row.unit.name)).toEqual([
      "system.service",
      "user.service",
    ]);
  });

  it("survives malformed payloads without touching state", () => {
    const mirror = new SystemdUnitsMirror();
    mirror.apply(
      JSON.stringify({
        type: "snapshot",
        scope: "system",
        chunk: 0,
        units: [unit("a.service", "active", "running")],
        last: true,
      }),
    );
    mirror.apply("not json");
    mirror.apply(JSON.stringify({ type: "snapshot" }));
    mirror.apply(JSON.stringify({ type: "change", scope: "system" }));
    mirror.apply(
      JSON.stringify({
        type: "change",
        scope: "system",
        added: [{ load: "loaded" }],
      }),
    );
    expect([...mirror.scopes.get("system")!.units.keys()]).toEqual([
      "a.service",
    ]);
  });
});

/** A channel whose peer is a script of replies, so paging can be tested. */
function fakeChannel() {
  const sent: string[] = [];
  let deliver: ((payload: Uint8Array) => void) | undefined;
  const encoder = new TextEncoder();
  const connection = {
    connectChannel: async (_name: string, options: any) => {
      deliver = options.onData;
      return {
        channelId: 2,
        name: "blit.systemd.v1",
        peer: "ext:1:1",
        metadata: new Uint8Array(),
        availableCredit: 1024n,
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
    },
  };
  return {
    connection,
    sent,
    reply: (message: unknown) =>
      deliver?.(encoder.encode(JSON.stringify(message))),
  };
}

const entry = (cursor: string, message: string) => ({
  cursor,
  realtime: "1787014660944726",
  priority: "6",
  unit: "sshd.service",
  pid: "1",
  message,
});

describe("journal queries", () => {
  it("correlates a chunked reply with its request", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const page = handle.logs({ unit: "sshd.service", limit: 2 });
    const request = JSON.parse(peer.sent.at(-1)!);
    expect(request).toMatchObject({
      type: "logs",
      unit: "sshd.service",
      limit: 2,
    });
    expect(typeof request.id).toBe("string");

    // Two chunks, and a stray reply for another id must not settle this one.
    peer.reply({
      type: "logs",
      id: "other",
      entries: [entry("x", "no")],
      last: true,
    });
    peer.reply({
      type: "logs",
      id: request.id,
      chunk: 0,
      entries: [entry("a", "one")],
      last: false,
    });
    peer.reply({
      type: "logs",
      id: request.id,
      chunk: 1,
      entries: [entry("b", "two")],
      last: true,
      more: true,
    });
    const resolved = await page;
    expect(resolved.entries.map((e) => e.message)).toEqual(["one", "two"]);
    expect(resolved.more).toBe(true);
  });

  it("rejects with journalctl's own words", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const page = handle.logs();
    const { id } = JSON.parse(peer.sent.at(-1)!);
    peer.reply({
      type: "error",
      id,
      message: "No journal files were opened due to insufficient permissions.",
    });
    await expect(page).rejects.toThrow(/insufficient permissions/);
  });

  it("keeps state messages away from the query router", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    peer.reply({
      type: "snapshot",
      scope: "system",
      chunk: 0,
      last: true,
      units: [
        { name: "a.service", load: "loaded", active: "active", sub: "running" },
      ],
    });
    expect(handle.scopes.get("system")?.units.size).toBe(1);
  });

  it("reads boots as records, not entries", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const pending = handle.boots();
    const { id } = JSON.parse(peer.sent.at(-1)!);
    peer.reply({
      type: "boots",
      id,
      entries: [
        { boot: "abc", index: "0", first: "1", last: "2" },
        { nope: true },
      ],
      last: true,
    });
    const boots = await pending;
    expect(boots).toHaveLength(1);
    expect(boots[0]!.boot).toBe("abc");
  });
});

describe("journal follow", () => {
  it("resumes from a cursor and leaves paging vocabulary out of it", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    handle.followLogs(
      {
        scope: "system",
        unit: "sshd.service",
        boot: "abc",
        limit: 200,
        direction: "backward",
        cursor: "s=1",
      },
      { onEntries: () => {}, onEnd: () => {} },
    );
    const request = JSON.parse(peer.sent.at(-1)!);
    expect(request).toMatchObject({
      type: "follow",
      unit: "sshd.service",
      cursor: "s=1",
    });
    // A tail runs in the boot doing the writing, and has no page to bound.
    expect(request.boot).toBeUndefined();
    expect(request.limit).toBeUndefined();
    expect(request.direction).toBeUndefined();
  });

  it("routes live batches to the sink, not the page router", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const seen: string[] = [];
    handle.followLogs(
      {},
      {
        onEntries: (page) => seen.push(...page.map((e) => e.message)),
        onEnd: () => {},
      },
    );
    const { id } = JSON.parse(peer.sent.at(-1)!);

    // A page query in flight alongside the tail must be settled by its own
    // reply — every follow batch carries `last`, so this is the trap.
    const page = handle.logs({ limit: 1 });
    const query = JSON.parse(peer.sent.at(-1)!);
    peer.reply({
      type: "logs",
      follow: true,
      id,
      entries: [entry("a", "tick")],
      last: true,
    });
    peer.reply({
      type: "logs",
      follow: true,
      id,
      entries: [entry("b", "tock")],
      last: true,
    });
    expect(seen).toEqual(["tick", "tock"]);

    peer.reply({
      type: "logs",
      id: query.id,
      entries: [entry("c", "page")],
      last: true,
    });
    expect((await page).entries.map((e) => e.message)).toEqual(["page"]);
  });

  it("stops feeding a tail the watcher already replaced", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const first: string[] = [];
    const second: string[] = [];
    handle.followLogs(
      {},
      {
        onEntries: (page) => first.push(...page.map((e) => e.message)),
        onEnd: () => {},
      },
    );
    const stale = JSON.parse(peer.sent.at(-1)!).id;
    handle.followLogs(
      { unit: "other.service" },
      {
        onEntries: (page) => second.push(...page.map((e) => e.message)),
        onEnd: () => {},
      },
    );
    const fresh = JSON.parse(peer.sent.at(-1)!).id;

    peer.reply({
      type: "logs",
      follow: true,
      id: stale,
      entries: [entry("a", "old")],
      last: true,
    });
    peer.reply({
      type: "logs",
      follow: true,
      id: fresh,
      entries: [entry("b", "new")],
      last: true,
    });
    expect(first).toEqual([]);
    expect(second).toEqual(["new"]);
  });

  it("says when a tail ended, and stops the stream when closed", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const ends: string[] = [];
    const tail = handle.followLogs(
      {},
      { onEntries: () => {}, onEnd: (message) => ends.push(message) },
    );
    const { id } = JSON.parse(peer.sent.at(-1)!);
    peer.reply({ type: "followEnd", id, message: "journal follow ended" });
    expect(ends).toEqual(["journal follow ended"]);

    // The tail is gone, so closing it must not tell the watcher to cancel a
    // follow that a later reader may have started in the meantime.
    tail.close();
    expect(peer.sent.at(-1)).not.toContain("unfollow");
  });

  it("cancels the watcher's stream when the reader closes it", async () => {
    const peer = fakeChannel();
    const handle = await openSystemdUnits(peer.connection as never);
    const tail = handle.followLogs(
      {},
      { onEntries: () => {}, onEnd: () => {} },
    );
    tail.close();
    expect(JSON.parse(peer.sent.at(-1)!)).toMatchObject({ type: "unfollow" });
  });
});

describe("unit filters", () => {
  const mirror = () => {
    const state = new SystemdUnitsMirror();
    for (const [scope, units] of [
      [
        "system",
        [
          unit("sshd.service", "active", "running"),
          unit("libk.timer", "active", "waiting"),
          unit("broken.service", "failed", "failed"),
        ],
      ],
      ["user", [unit("pipewire.service", "active", "running")]],
    ] as const) {
      state.apply(
        JSON.stringify({
          type: "snapshot",
          scope,
          chunk: 0,
          units,
          last: true,
        }),
      );
    }
    return state;
  };

  it("keeps everything when nothing is asked for", () => {
    expect(filterUnits(mirror().scopes).map((row) => row.name)).toEqual([
      "broken.service",
      "libk.timer",
      "pipewire.service",
      "sshd.service",
    ]);
  });

  it("filters by scope, state and type independently", () => {
    const scopes = mirror().scopes;
    expect(
      filterUnits(scopes, { scope: "user" }).map((row) => row.name),
    ).toEqual(["pipewire.service"]);
    expect(
      filterUnits(scopes, { state: "failed" }).map((row) => row.name),
    ).toEqual(["broken.service"]);
    expect(
      filterUnits(scopes, { type: "timer" }).map((row) => row.name),
    ).toEqual(["libk.timer"]);
    // A type filter matches the suffix, not the substring: `.service` must not
    // pick up a unit merely called "service-something".
    expect(
      filterUnits(scopes, { type: "service" }).map((row) => row.name),
    ).toEqual(["broken.service", "pipewire.service", "sshd.service"]);
  });

  it("combines filters and searches name and description", () => {
    const scopes = mirror().scopes;
    expect(
      filterUnits(scopes, {
        scope: "system",
        state: "active",
        type: "service",
      }).map((row) => row.name),
    ).toEqual(["sshd.service"]);
    expect(
      filterUnits(scopes, { search: "SSHD" }).map((row) => row.name),
    ).toEqual(["sshd.service"]);
    expect(
      filterUnits(scopes, { search: "nothing-matches-this" }).map(
        (row) => row.name,
      ),
    ).toEqual([]);
    // The description is searchable too; the fixture describes each unit.
    expect(
      filterUnits(scopes, { search: "pipewire.service unit" }).map(
        (row) => row.name,
      ),
    ).toEqual(["pipewire.service"]);
  });

  it("offers only the states this server actually has", () => {
    expect(unitStates(mirror().scopes)).toEqual(["active", "failed"]);
  });
});
