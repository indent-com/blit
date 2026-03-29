import { describe, it, expect, beforeEach, vi } from "vitest";
import { BlitConnection } from "../BlitConnection";
import { MockTransport } from "./mock-transport";
import type { BlitWasmModule } from "../TerminalStore";
import {
  C2S_INPUT,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_CREATE2,
  CREATE2_HAS_COMMAND,
  FEATURE_CREATE_NONCE,
  FEATURE_RESIZE_BATCH,
  FEATURE_RESTART,
} from "../types";

class FakeTerminal {
  constructor(_r: number, _c: number, _pw: number, _ph: number) {}
  set_font_family(_f: string) {}
  set_font_size(_s: number) {}
  set_default_colors(..._a: number[]) {}
  set_ansi_color(..._a: number[]) {}
  feed_compressed(_d: Uint8Array) {}
  invalidate_render_cache() {}
  title() {
    return "";
  }
  free() {}
}

const wasm = { Terminal: FakeTerminal } as unknown as BlitWasmModule;

function createConnection(transport?: MockTransport) {
  const t = transport ?? new MockTransport();
  const conn = new BlitConnection({
    id: "test",
    transport: t,
    wasm,
    autoConnect: false,
  });
  return { conn, transport: t };
}

describe("BlitConnection", () => {
  let transport: MockTransport;
  let conn: BlitConnection;

  beforeEach(() => {
    ({ conn, transport } = createConnection());
  });

  // --- Status tracking ---

  it("starts with transport status", () => {
    expect(conn.getSnapshot().status).toBe("connected");
  });

  it("tracks status changes", () => {
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().status).toBe("disconnected");
  });

  it("tracks retryCount on failed connection attempts", () => {
    expect(conn.getSnapshot().retryCount).toBe(0);
    // Simulate: connected → disconnected (not from connecting, no increment)
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().retryCount).toBe(0);
    // Retry: connecting → error (failed attempt)
    transport.setStatus("connecting");
    transport.setStatus("error");
    expect(conn.getSnapshot().retryCount).toBe(1);
    // Another retry: connecting → disconnected (failed attempt)
    transport.setStatus("connecting");
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().retryCount).toBe(2);
    // Successful reconnect resets
    transport.setStatus("connecting");
    transport.setStatus("connected");
    expect(conn.getSnapshot().retryCount).toBe(0);
  });

  // --- Session tracking via CREATED/CLOSED ---

  it("tracks CREATED", () => {
    transport.pushCreated(5, "editor");
    const sessions = conn.getSnapshot().sessions;
    expect(sessions.length).toBe(1);
    expect(sessions[0].tag).toBe("editor");
    expect(sessions[0].state).toBe("active");
  });

  it("tracks CREATED with empty tag", () => {
    transport.pushCreated(1);
    expect(conn.getSnapshot().sessions[0].tag).toBe("");
  });

  it("marks session closed on CLOSED", () => {
    transport.pushCreated(1, "x");
    transport.pushClosed(1);
    expect(conn.getSnapshot().sessions[0].state).toBe("closed");
  });

  it("ignores CLOSED for unknown ptyId", () => {
    transport.pushCreated(1, "x");
    transport.pushClosed(99);
    expect(conn.getSnapshot().sessions[0].state).toBe("active");
  });

  // --- Titles ---

  it("updates title on TITLE", () => {
    transport.pushCreated(1, "");
    transport.pushTitle(1, "bash");
    expect(conn.getSnapshot().sessions[0].title).toBe("bash");
  });

  it("ignores TITLE for unknown ptyId", () => {
    transport.pushCreated(1, "");
    transport.pushTitle(99, "nope");
    expect(conn.getSnapshot().sessions[0].title).toBeNull();
  });

  // --- LIST reconciliation ---

  it("becomes ready after LIST", () => {
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    expect(conn.getSnapshot().ready).toBe(true);
    expect(conn.getSnapshot().sessions.length).toBe(1);
  });

  it("reconciles LIST — marks missing PTYs as closed, adds new", () => {
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
    ]);
    transport.pushList([
      { ptyId: 2, tag: "b" },
      { ptyId: 3, tag: "c" },
    ]);
    const s = conn.getSnapshot().sessions;
    expect(s.find((x) => x.tag === "a")?.state).toBe("closed");
    expect(s.find((x) => x.tag === "b")?.state).toBe("active");
    expect(s.find((x) => x.tag === "c")?.state).toBe("active");
  });

  it("preserves title across LIST reconciliation", () => {
    transport.pushList([{ ptyId: 1, tag: "" }]);
    transport.pushTitle(1, "vim");
    transport.pushList([{ ptyId: 1, tag: "" }]);
    expect(conn.getSnapshot().sessions[0].title).toBe("vim");
  });

  it("multiple CREATED accumulate", () => {
    transport.pushList([]);
    transport.pushCreated(1, "a");
    transport.pushCreated(2, "b");
    transport.pushCreated(3, "c");
    expect(conn.getSnapshot().sessions.map((s) => s.tag)).toEqual([
      "a", "b", "c",
    ]);
  });

  // --- Focus ---

  it("focusedSessionId is null for empty LIST", () => {
    transport.pushList([]);
    expect(conn.getSnapshot().focusedSessionId).toBeNull();
  });

  it("focusedSessionId is null for non-empty LIST", () => {
    transport.pushList([
      { ptyId: 5, tag: "a" },
      { ptyId: 6, tag: "b" },
    ]);
    const snap = conn.getSnapshot();
    expect(snap.focusedSessionId).toBeNull();
  });

  it("focusedSessionId moves to next active on CLOSED", () => {
    transport.pushList([{ ptyId: 1, tag: "first" }, { ptyId: 2, tag: "second" }]);
    const s1 = conn.getSnapshot().sessions.find((s) => s.tag === "first")!;
    conn.focusSession(s1.id);
    transport.pushClosed(1);
    const snap = conn.getSnapshot();
    const focused = snap.sessions.find((s) => s.id === snap.focusedSessionId);
    expect(focused?.tag).toBe("second");
  });

  it("focusedSessionId becomes null when all sessions close", () => {
    transport.pushList([{ ptyId: 1 }]);
    transport.pushClosed(1);
    expect(conn.getSnapshot().focusedSessionId).toBeNull();
  });

  // --- createSession ---

  it("createSession sends C2S_CREATE2", () => {
    conn.createSession({ rows: 24, cols: 80, tag: "test" });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    expect(msg).toBeDefined();
    expect(msg[3] | (msg[4] << 8)).toBe(24);
    expect(msg[5] | (msg[6] << 8)).toBe(80);
  });

  it("createSession with command sets features", () => {
    conn.createSession({ rows: 24, cols: 80, tag: "bg", command: "make" });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    expect(msg[7]).toBe(CREATE2_HAS_COMMAND);
  });

  it("createSession resolves via S2C_CREATED_N when nonce supported", async () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    const promise = conn.createSession({ rows: 24, cols: 80, tag: "test" });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    const nonce = msg[1] | (msg[2] << 8);
    transport.pushCreatedN(nonce, 42, "test");
    const session = await promise;
    expect(session.tag).toBe("test");
  });

  it("createSession falls back to FIFO via S2C_CREATED", async () => {
    const promise = conn.createSession({ rows: 24, cols: 80, tag: "test" });
    transport.pushCreated(42, "test");
    const session = await promise;
    expect(session.tag).toBe("test");
  });

  it("createSession rejects on disconnect", async () => {
    const promise = conn.createSession({ rows: 24, cols: 80 });
    transport.setStatus("disconnected");
    await expect(promise).rejects.toThrow(/disconnected/);
  });

  // --- closeSession ---

  it("closeSession sends CLOSE", async () => {
    transport.pushList([{ ptyId: 7, tag: "" }]);
    const session = conn.getSnapshot().sessions[0];
    conn.closeSession(session.id);
    const msg = transport.sent.find((m) => m[0] === C2S_CLOSE)!;
    expect(msg).toBeDefined();
    expect(msg[1] | (msg[2] << 8)).toBe(7);
  });

  it("closeSession resolves on S2C_CLOSED", async () => {
    transport.pushList([{ ptyId: 7, tag: "" }]);
    const session = conn.getSnapshot().sessions[0];
    const promise = conn.closeSession(session.id);
    transport.pushClosed(7);
    await promise;
  });

  it("closeSession resolves on disconnect", async () => {
    transport.pushList([{ ptyId: 7, tag: "" }]);
    const session = conn.getSnapshot().sessions[0];
    const promise = conn.closeSession(session.id);
    transport.setStatus("disconnected");
    await promise;
  });

  // --- Send helpers ---

  it("sendInput sends INPUT with session ptyId", () => {
    transport.pushCreated(3, "");
    const session = conn.getSnapshot().sessions[0];
    conn.sendInput(session.id, new Uint8Array([0x6c, 0x73]));
    const msg = transport.sent.find((m) => m[0] === C2S_INPUT)!;
    expect(msg[1] | (msg[2] << 8)).toBe(3);
    expect(msg[3]).toBe(0x6c);
    expect(msg[4]).toBe(0x73);
  });

  it("resizeSession sends RESIZE", () => {
    transport.pushCreated(1, "");
    const session = conn.getSnapshot().sessions[0];
    conn.resizeSession(session.id, 24, 80);
    const msg = transport.sent.find((m) => m[0] === C2S_RESIZE)!;
    expect(msg[1] | (msg[2] << 8)).toBe(1);
    expect(msg[3] | (msg[4] << 8)).toBe(24);
    expect(msg[5] | (msg[6] << 8)).toBe(80);
  });

  it("resizeSessions batches RESIZE entries when supported", () => {
    transport.pushHello(1, FEATURE_RESIZE_BATCH);
    transport.pushCreated(1, "");
    transport.pushCreated(2, "");
    const [first, second] = conn.getSnapshot().sessions;
    conn.resizeSessions([
      { sessionId: first.id, rows: 24, cols: 80 },
      { sessionId: second.id, rows: 40, cols: 120 },
    ]);
    const msg = transport.sent[transport.sent.length - 1]!;
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg.length).toBe(13);
    expect(msg[1] | (msg[2] << 8)).toBe(1);
    expect(msg[3] | (msg[4] << 8)).toBe(24);
    expect(msg[5] | (msg[6] << 8)).toBe(80);
    expect(msg[7] | (msg[8] << 8)).toBe(2);
    expect(msg[9] | (msg[10] << 8)).toBe(40);
    expect(msg[11] | (msg[12] << 8)).toBe(120);
  });

  it("resizeSessions falls back to single-entry RESIZE messages", () => {
    transport.pushCreated(1, "");
    transport.pushCreated(2, "");
    const [first, second] = conn.getSnapshot().sessions;
    const before = transport.sent.length;
    conn.resizeSessions([
      { sessionId: first.id, rows: 24, cols: 80 },
      { sessionId: second.id, rows: 40, cols: 120 },
    ]);
    const sent = transport.sent.slice(before).filter((msg) => msg[0] === C2S_RESIZE);
    expect(sent).toHaveLength(2);
    expect(sent[0]![1] | (sent[0]![2] << 8)).toBe(1);
    expect(sent[1]![1] | (sent[1]![2] << 8)).toBe(2);
  });

  it("clearSessionSizes sends unset-view-size resize entries when supported", () => {
    transport.pushHello(1, FEATURE_RESIZE_BATCH);
    transport.pushCreated(1, "");
    transport.pushCreated(2, "");
    const [first, second] = conn.getSnapshot().sessions;
    conn.clearSessionSizes([first.id, second.id]);
    const msg = transport.sent[transport.sent.length - 1]!;
    expect(msg[0]).toBe(C2S_RESIZE);
    expect(msg.length).toBe(13);
    expect(msg[1] | (msg[2] << 8)).toBe(1);
    expect(msg[3] | (msg[4] << 8)).toBe(0);
    expect(msg[5] | (msg[6] << 8)).toBe(0);
    expect(msg[7] | (msg[8] << 8)).toBe(2);
    expect(msg[9] | (msg[10] << 8)).toBe(0);
    expect(msg[11] | (msg[12] << 8)).toBe(0);
  });

  it("clearSessionSizes is ignored when extended resize semantics are unavailable", () => {
    transport.pushCreated(1, "");
    const session = conn.getSnapshot().sessions[0];
    const before = transport.sent.length;
    conn.clearSessionSize(session.id);
    expect(transport.sent).toHaveLength(before);
  });

  it("scrollSession sends SCROLL", () => {
    transport.pushCreated(2, "");
    const session = conn.getSnapshot().sessions[0];
    conn.scrollSession(session.id, 100);
    const msg = transport.sent.find((m) => m[0] === C2S_SCROLL)!;
    expect(msg[1] | (msg[2] << 8)).toBe(2);
    const offset = msg[3] | (msg[4] << 8) | (msg[5] << 16) | (msg[6] << 24);
    expect(offset).toBe(100);
  });

  it("focusSession sends FOCUS", () => {
    transport.pushCreated(9, "");
    const session = conn.getSnapshot().sessions[0];
    conn.focusSession(session.id);
    const msg = transport.sent.find((m) => m[0] === C2S_FOCUS)!;
    expect(msg[1] | (msg[2] << 8)).toBe(9);
  });

  // --- S2C_HELLO ---

  it("closes transport on hello with version > PROTOCOL_VERSION", () => {
    transport.pushHello(2, FEATURE_CREATE_NONCE);
    expect(conn.getSnapshot().status).toBe("disconnected");
  });

  it("accepts hello with version 1", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    expect(conn.getSnapshot().status).toBe("connected");
  });

  it("supportsRestart reflects FEATURE_RESTART", () => {
    transport.pushHello(1, FEATURE_RESTART);
    expect(conn.getSnapshot().supportsRestart).toBe(true);
  });

  // --- Unicode ---

  it("handles unicode tag in CREATED", () => {
    transport.pushCreated(1, "日本語");
    expect(conn.getSnapshot().sessions[0].tag).toBe("日本語");
  });

  it("handles unicode title in TITLE", () => {
    transport.pushCreated(1, "");
    transport.pushTitle(1, "émacs");
    expect(conn.getSnapshot().sessions[0].title).toBe("émacs");
  });

  it("handles LIST with unicode tags", () => {
    transport.pushList([{ ptyId: 1, tag: "🚀" }]);
    expect(conn.getSnapshot().sessions[0].tag).toBe("🚀");
  });

  // --- Ignores malformed messages ---

  it("ignores too-short CREATED", () => {
    transport.push(new Uint8Array([0x01, 0x05]));
    expect(conn.getSnapshot().sessions.length).toBe(0);
  });

  it("ignores empty messages", () => {
    transport.push(new Uint8Array([]));
    expect(conn.getSnapshot().sessions.length).toBe(0);
  });

  // --- Subscriber notifications ---

  it("notifies subscribers on state changes", () => {
    const listener = vi.fn();
    conn.subscribe(listener);
    transport.pushCreated(1, "");
    expect(listener).toHaveBeenCalled();
  });

  it("unsubscribe stops notifications", () => {
    const listener = vi.fn();
    const unsub = conn.subscribe(listener);
    unsub();
    transport.pushCreated(1, "");
    expect(listener).not.toHaveBeenCalled();
  });

  // --- Dispose ---

  it("dispose cleans up", () => {
    conn.dispose();
    // Should not crash on further transport events
    transport.pushCreated(1, "");
    expect(conn.getSnapshot().sessions.length).toBe(0);
  });
});

describe("BlitConnection — advanced scenarios", () => {
  it("handles rapid create/close/create cycle", () => {
    const { conn, transport } = createConnection();
    transport.pushList([]);
    transport.pushCreated(1, "a");
    transport.pushClosed(1);
    transport.pushCreated(2, "b");
    const s = conn.getSnapshot().sessions;
    expect(s.find((x) => x.tag === "a")?.state).toBe("closed");
    expect(s.find((x) => x.tag === "b")?.state).toBe("active");
  });

  it("handles duplicate CREATED for same pty", () => {
    const { conn, transport } = createConnection();
    transport.pushList([]);
    transport.pushCreated(1, "first");
    transport.pushCreated(1, "second");
    // Second CREATED updates the existing session's tag
    const active = conn.getSnapshot().sessions.filter((s) => s.state === "active");
    expect(active.length).toBe(1);
    expect(active[0].tag).toBe("second");
  });

  it("handles CLOSED then re-CREATED with same ptyId", () => {
    const { conn, transport } = createConnection();
    transport.pushList([]);
    transport.pushCreated(1, "v1");
    transport.pushClosed(1);
    transport.pushCreated(1, "v2");
    const active = conn
      .getSnapshot()
      .sessions.filter((s) => s.tag === "v2" && s.state === "active");
    expect(active.length).toBe(1);
    expect(active[0].tag).toBe("v2");
  });

  it("title updates are independent per pty", () => {
    const { conn, transport } = createConnection();
    transport.pushList([
      { ptyId: 1, tag: "s1" },
      { ptyId: 2, tag: "s2" },
    ]);
    transport.pushTitle(1, "vim");
    transport.pushTitle(2, "bash");
    const s = conn.getSnapshot().sessions;
    expect(s.find((x) => x.tag === "s1")?.title).toBe("vim");
    expect(s.find((x) => x.tag === "s2")?.title).toBe("bash");
  });

  it("title can be updated multiple times", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1 }]);
    transport.pushTitle(1, "a");
    transport.pushTitle(1, "b");
    transport.pushTitle(1, "c");
    expect(conn.getSnapshot().sessions[0].title).toBe("c");
  });

  it("title can be set to empty string", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1 }]);
    transport.pushTitle(1, "vim");
    transport.pushTitle(1, "");
    expect(conn.getSnapshot().sessions[0].title).toBe("");
  });

  it("LIST with same entries is idempotent", () => {
    const { conn, transport } = createConnection();
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
    ]);
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
    ]);
    expect(conn.getSnapshot().sessions.map((s) => s.tag)).toEqual(["a", "b"]);
    expect(conn.getSnapshot().sessions.every((s) => s.state === "active")).toBe(
      true,
    );
  });

  it("empty LIST marks everything closed", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1 }, { ptyId: 2 }, { ptyId: 3 }]);
    transport.pushList([]);
    expect(conn.getSnapshot().sessions.every((s) => s.state === "closed")).toBe(
      true,
    );
  });

  it("handles high pty IDs (u16 range)", () => {
    const { conn, transport } = createConnection();
    transport.pushList([]);
    transport.pushCreated(65535, "max");
    transport.pushCreated(256, "mid");
    expect(
      conn.getSnapshot().sessions.find((s) => s.tag === "max")?.tag,
    ).toBe("max");
    expect(conn.getSnapshot().sessions.find((s) => s.tag === "mid")?.tag).toBe(
      "mid",
    );
  });

  it("handles 100 sessions", () => {
    const { conn, transport } = createConnection();
    const entries = Array.from({ length: 100 }, (_, i) => ({
      ptyId: i,
      tag: `tag-${i}`,
    }));
    transport.pushList(entries);
    expect(conn.getSnapshot().sessions.length).toBe(100);
    expect(conn.getSnapshot().sessions[50].tag).toBe("tag-50");
  });

  it("sessions persist across transport disconnect", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().sessions.length).toBe(1);
    expect(conn.getSnapshot().sessions[0].state).toBe("active");
  });

  it("sessions reconcile on reconnect LIST", () => {
    const { conn, transport } = createConnection();
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
    ]);
    transport.pushTitle(1, "vim");
    transport.setStatus("disconnected");
    transport.setStatus("connected");
    transport.pushList([
      { ptyId: 2, tag: "b" },
      { ptyId: 3, tag: "c" },
    ]);
    const s = conn.getSnapshot().sessions;
    expect(s.find((x) => x.tag === "a")?.state).toBe("closed");
    expect(s.find((x) => x.tag === "b")?.state).toBe("active");
    expect(s.find((x) => x.tag === "c")?.state).toBe("active");
    expect(s.find((x) => x.tag === "a")?.title).toBe("vim");
  });

  it("handles emoji tags and titles", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "🚀🔥" }]);
    transport.pushTitle(1, "💻 terminal — ñoño");
    const s = conn.getSnapshot().sessions[0];
    expect(s.tag).toBe("🚀🔥");
    expect(s.title).toBe("💻 terminal — ñoño");
  });

  it("handles CJK tags", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "日本語ターミナル" }]);
    expect(conn.getSnapshot().sessions[0].tag).toBe("日本語ターミナル");
  });

  it("ready stays true after multiple LISTs", () => {
    const { conn, transport } = createConnection();
    transport.pushList([]);
    expect(conn.getSnapshot().ready).toBe(true);
    transport.pushList([{ ptyId: 1 }]);
    expect(conn.getSnapshot().ready).toBe(true);
    transport.pushList([]);
    expect(conn.getSnapshot().ready).toBe(true);
  });

  it("operations before LIST are safe", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(1, "early");
    transport.pushTitle(1, "title");
    transport.pushClosed(99);
    expect(conn.getSnapshot().ready).toBe(false);
    expect(conn.getSnapshot().sessions.length).toBe(1);
    expect(conn.getSnapshot().sessions[0].title).toBe("title");
  });

  it("focusedSessionId survives LIST reconciliation when pty still exists", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "a" }, { ptyId: 2, tag: "b" }, { ptyId: 3, tag: "c" }]);
    const s2 = conn.getSnapshot().sessions.find((s) => s.tag === "b")!;
    conn.focusSession(s2.id);
    transport.pushList([{ ptyId: 2, tag: "b" }, { ptyId: 3, tag: "c" }]);
    const snap = conn.getSnapshot();
    const focused = snap.sessions.find((s) => s.id === snap.focusedSessionId);
    expect(focused?.tag).toBe("b");
  });

  it("focusedSessionId falls back when focused pty removed from LIST", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "a" }, { ptyId: 2, tag: "b" }]);
    const s1 = conn.getSnapshot().sessions.find((s) => s.tag === "a")!;
    conn.focusSession(s1.id);
    transport.pushList([{ ptyId: 2, tag: "b" }]);
    const snap = conn.getSnapshot();
    const focused = snap.sessions.find((s) => s.id === snap.focusedSessionId);
    expect(focused?.tag).toBe("b");
  });
});
