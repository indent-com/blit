import { describe, expect, it } from "vitest";
import { openSession, SessionMirror } from "../session";

const encode = (value: unknown): Uint8Array =>
  new TextEncoder().encode(JSON.stringify(value));

const state = (apps: unknown[], catalog?: unknown[]): Uint8Array =>
  encode(
    catalog === undefined
      ? { type: "state", apps }
      : { type: "state", apps, catalog },
  );

describe("SessionMirror", () => {
  it("is not ready until state arrives", () => {
    const mirror = new SessionMirror();
    expect(mirror.ready).toBe(false);
    expect(mirror.apps).toEqual([]);
    mirror.apply(state([]));
    expect(mirror.ready).toBe(true);
  });

  it("sorts by display name, not by id", () => {
    const mirror = new SessionMirror();
    mirror.apply(
      state([
        { id: "zed", name: "Alpha", enabled: true, phase: "running" },
        { id: "alpha", name: "Zed", enabled: true, phase: "running" },
      ]),
    );
    expect(mirror.apps.map((app) => app.id)).toEqual(["zed", "alpha"]);
  });

  /** The catalog is the larger half and rides only a greeting or a resync, so
   *  an ordinary update must not be read as "everything was uninstalled". */
  it("keeps the catalog across an update that omits it", () => {
    const mirror = new SessionMirror();
    mirror.apply(state([], [{ id: "a", name: "A" }]));
    expect(mirror.catalog).toHaveLength(1);
    mirror.apply(
      state([{ id: "a", name: "A", enabled: true, phase: "running" }]),
    );
    expect(mirror.catalog).toHaveLength(1);
    expect(mirror.apps).toHaveLength(1);
  });

  it("defaults missing fields rather than dropping the row", () => {
    const mirror = new SessionMirror();
    mirror.apply(state([{ id: "bare" }]));
    expect(mirror.apps[0]).toMatchObject({
      id: "bare",
      name: "bare",
      enabled: false,
      phase: "stopped",
      failures: 0,
      windows: 0,
    });
    expect(mirror.apps[0]?.socket).toBeUndefined();
  });

  it("drops rows with no id, and unknown phases fall back to stopped", () => {
    const mirror = new SessionMirror();
    mirror.apply(
      state([
        { id: "", name: "nameless" },
        { name: "no id at all" },
        { id: "ok", phase: "wat" },
      ]),
    );
    expect(mirror.apps.map((app) => app.id)).toEqual(["ok"]);
    expect(mirror.apps[0]?.phase).toBe("stopped");
  });

  /** A panel is not the place to surface a parser disagreement: a malformed
   *  message must leave the last good state standing. */
  it("ignores malformed payloads and foreign message types", () => {
    const mirror = new SessionMirror();
    mirror.apply(state([{ id: "keep", name: "Keep", phase: "running" }]));
    const revision = mirror.revision;

    mirror.apply(new TextEncoder().encode("not json at all"));
    mirror.apply(encode({ type: "hello" }));
    mirror.apply(encode([1, 2, 3]));
    mirror.apply(new Uint8Array());

    expect(mirror.apps.map((app) => app.id)).toEqual(["keep"]);
    expect(mirror.revision).toBe(revision);
  });

  it("notifies subscribers once per applied message", () => {
    const mirror = new SessionMirror();
    let calls = 0;
    const stop = mirror.subscribe(() => calls++);
    mirror.apply(state([]));
    mirror.apply(state([{ id: "a", phase: "running" }]));
    expect(calls).toBe(2);
    stop();
    mirror.apply(state([]));
    expect(calls).toBe(2);
  });
});

/**
 * The four verbs are two pairs, and the wire is the only place the difference
 * is expressed: `stop` leaves intent alone, `disable` does not. A panel that
 * sent the wrong one would look right and quietly forget an application.
 */
describe("openSession", () => {
  const fakeConnection = () => {
    const sent: string[] = [];
    return {
      sent,
      connectChannel: async () => ({
        id: 2,
        send: (payload: string | Uint8Array) => {
          sent.push(
            typeof payload === "string"
              ? payload
              : new TextDecoder().decode(payload),
          );
        },
        close: () => {},
      }),
    } as unknown as { sent: string[] } & Parameters<typeof openSession>[0];
  };

  it("sends one line per verb, naming the application", async () => {
    const connection = fakeConnection();
    const session = await openSession(connection);
    session.enable("org.gnome.Nautilus");
    session.disable("org.gnome.Nautilus");
    session.start("org.gnome.Nautilus");
    session.stop("org.gnome.Nautilus");
    session.forget("org.gnome.Nautilus");
    session.resync();
    expect(connection.sent).toEqual([
      "enable org.gnome.Nautilus",
      "disable org.gnome.Nautilus",
      "start org.gnome.Nautilus",
      "stop org.gnome.Nautilus",
      "forget org.gnome.Nautilus",
      "resync",
    ]);
  });

  it("stops sending once closed", async () => {
    const connection = fakeConnection();
    const session = await openSession(connection);
    session.close();
    session.start("a");
    expect(connection.sent).toEqual([]);
  });
});
