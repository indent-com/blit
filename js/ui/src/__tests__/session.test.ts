import { describe, expect, it, vi } from "vitest";
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

  /** Three states, not two: a row has to be able to tell "no artwork exists"
   *  from "the answer has not arrived", or it re-asks forever. The supervisor
   *  answers where the artwork is; the bytes are the panel's own errand. */
  it("records where an icon is, and records its absence too", () => {
    const mirror = new SessionMirror();
    expect(mirror.icon("gimp")).toBeUndefined();
    expect(mirror.path("gimp")).toBeUndefined();

    mirror.apply(
      encode({ type: "icon", id: "gimp", path: "/i/128x128/apps/gimp.png" }),
    );
    expect(mirror.path("gimp")).toBe("/i/128x128/apps/gimp.png");
    // Located, not yet read: the row still has no URL to draw.
    expect(mirror.icon("gimp")).toBeUndefined();
    expect(mirror.unread()).toEqual([
      { id: "gimp", path: "/i/128x128/apps/gimp.png" },
    ]);

    mirror.setIcon("gimp", "blob:whatever");
    expect(mirror.icon("gimp")).toBe("blob:whatever");
    expect(mirror.unread()).toEqual([]);

    // No `path` field is the answer "there is none", and it is final.
    mirror.apply(encode({ type: "icon", id: "bare" }));
    expect(mirror.path("bare")).toBeNull();
    expect(mirror.icon("bare")).toBeNull();
    expect(mirror.unread()).toEqual([]);
  });

  /** An icon message carries no apps, and reading it as state would empty the
   *  list every time a row's artwork arrived. */
  it("an icon message leaves the application list alone", () => {
    const mirror = new SessionMirror();
    mirror.apply(
      state(
        [{ id: "a", name: "A", phase: "running" }],
        [{ id: "a", name: "A" }],
      ),
    );
    mirror.apply(encode({ type: "icon", id: "a", path: "/i/a.svg" }));
    expect(mirror.apps.map((app) => app.id)).toEqual(["a"]);
    expect(mirror.catalog).toHaveLength(1);
  });

  /** A path is a path: anything that is not a non-empty string is the answer
   *  "there is nothing to draw", never something handed to a reader. */
  it("refuses a path that is not one", () => {
    const mirror = new SessionMirror();
    mirror.apply(encode({ type: "icon", id: "a", path: 42 }));
    expect(mirror.path("a")).toBeNull();
    expect(mirror.icon("a")).toBeNull();
    mirror.apply(encode({ type: "icon", id: "b", path: "" }));
    expect(mirror.icon("b")).toBeNull();
    // An id-less message answers for nothing and is dropped whole.
    mirror.apply(encode({ type: "icon", path: "/i/x.png" }));
    expect(mirror.icon("")).toBeUndefined();
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
    // Captured so a test can answer, which is the only way to reach the mirror
    // the way the wire does.
    const inbound: { deliver?: (payload: Uint8Array) => void } = {};
    return {
      sent,
      inbound,
      connectChannel: async (
        _name: string,
        options: { onData?: (payload: Uint8Array) => void } = {},
      ) => {
        inbound.deliver = options.onData;
        return {
          id: 2,
          send: (payload: string | Uint8Array) => {
            sent.push(
              typeof payload === "string"
                ? payload
                : new TextDecoder().decode(payload),
            );
          },
          close: () => {},
        };
      },
    } as unknown as {
      sent: string[];
      inbound: { deliver?: (payload: Uint8Array) => void };
    } & Parameters<typeof openSession>[0];
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

  /** Each request is a child process on the far end, and a scrolling list
   *  reveals rows a few at a time. Coalescing is what makes a flick of the
   *  wheel one round trip instead of six; the dedup is what keeps a redraw
   *  from being a request at all. */
  it("coalesces icon requests, asks once per id, and batches what it sends", async () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const session = await openSession(connection);
      session.requestIcons(["a", "b"]);
      session.requestIcons(["b", "c"]);
      expect(
        connection.sent,
        "nothing goes out before the window closes",
      ).toEqual([]);
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual(["icons a\nb\nc"]);

      // Over one batch: split, because the extension refuses a longer request.
      connection.sent.length = 0;
      session.requestIcons(Array.from({ length: 49 }, (_, at) => `app${at}`));
      vi.advanceTimersByTime(200);
      expect(connection.sent).toHaveLength(2);
      expect(connection.sent[0]?.split("\n")).toHaveLength(48);
      expect(connection.sent[1]).toBe("icons app48");

      // Steam names hundreds of its entries "3DMark Demo.desktop", so a space
      // in an id is ordinary and must survive the batching.
      connection.sent.length = 0;
      session.requestIcons(["3DMark Demo", ""]);
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual(["icons 3DMark Demo"]);

      // A panel closed inside the window must not send after it.
      connection.sent.length = 0;
      session.requestIcons(["late"]);
      session.close();
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual([]);
    } finally {
      vi.useRealTimers();
    }
  });

  /** The supervisor bounds what it will queue for one panel, so an answer can
   *  be lost. Without an expiry the id stays marked asked and that row keeps
   *  its placeholder for the life of the channel. */
  it("asks again for an id whose answer never came, but not for one answered", async () => {
    vi.useFakeTimers();
    try {
      const connection = fakeConnection();
      const session = await openSession(connection);
      session.requestIcons(["lost", "found"]);
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual(["icons lost\nfound"]);

      // One of the two is answered; the other never is.
      connection.inbound.deliver?.(
        encode({ type: "icon", id: "found", path: "/i/found.png" }),
      );

      // Still inside the window: neither is asked again.
      connection.sent.length = 0;
      session.requestIcons(["lost", "found"]);
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual([]);

      // Past it: only the one still without an answer.
      vi.advanceTimersByTime(9000);
      session.requestIcons(["lost", "found"]);
      vi.advanceTimersByTime(200);
      expect(connection.sent).toEqual(["icons lost"]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("stops sending once closed", async () => {
    const connection = fakeConnection();
    const session = await openSession(connection);
    session.close();
    session.start("a");
    expect(connection.sent).toEqual([]);
  });
});
