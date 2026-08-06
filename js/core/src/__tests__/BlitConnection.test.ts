import { describe, it, expect, beforeEach, vi } from "vitest";
import { BlitConnection } from "../BlitConnection";
import { MockTransport } from "./mock-transport";
import type { BlitWasmModule } from "../TerminalStore";
import {
  C2S_GIT_LOG_ACK,
  C2S_GIT_LOG_UNWATCH,
  C2S_GIT_LOG_WATCH,
  C2S_GIT_OPEN,
  C2S_GIT_RESOLVE,
  FEATURE_GIT,
  GIT_OID_FORMAT_SHA1,
  GIT_STATUS_OK,
  S2C_GIT_REPO,
  type GitLogPage,
  type GitRepoHandle,
  msgGitLogPage,
  msgGitResolveResp,
} from "../git";
import {
  C2S_INPUT,
  C2S_RESIZE,
  C2S_SCROLL,
  C2S_FOCUS,
  C2S_CLOSE,
  C2S_COPY_RANGE,
  C2S_CREATE2,
  CREATE2_HAS_COMMAND,
  CREATE2_HAS_CWD,
  CREATE2_WANT_STATUS,
  FEATURE_CREATE_NONCE,
  FEATURE_CREATE_STATUS,
  FEATURE_RESIZE_BATCH,
  FEATURE_RESTART,
  FRAGMENT_FLAG_LAST,
  S2C_FRAGMENT,
  S2C_PING,
  C2S_SURFACE_SUBSCRIBE,
  C2S_SURFACE_RESIZE,
  STATUS_BUDGET,
  STATUS_OTHER,
} from "../types";
import { FS_MAX_DECOMPRESSED } from "../fs";

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
    // MockTransport starts as "connected" but since the blit handshake
    // has not produced any server frames, the snapshot reports "authenticating".
    expect(conn.getSnapshot().status).toBe("authenticating");
  });

  it("tracks status changes", () => {
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().status).toBe("disconnected");
  });

  it("tracks retryCount on failed connection attempts", () => {
    expect(conn.getSnapshot().retryCount).toBe(0);
    // Simulate: authenticating → disconnected (handshake never completed)
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().retryCount).toBe(1);
    // Retry: connecting → error (failed attempt)
    transport.setStatus("connecting");
    transport.setStatus("error");
    expect(conn.getSnapshot().retryCount).toBe(2);
    // Another retry: connecting → disconnected (failed attempt)
    transport.setStatus("connecting");
    transport.setStatus("disconnected");
    expect(conn.getSnapshot().retryCount).toBe(3);
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

  it("becomes ready after READY", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    expect(conn.getSnapshot().ready).toBe(false);
    // S2C_LIST proves terminal state is available before READY.
    expect(conn.getSnapshot().status).toBe("connected");
    transport.pushReady();
    expect(conn.getSnapshot().ready).toBe(true);
    // S2C_READY keeps status "connected" and marks the initial burst complete.
    expect(conn.getSnapshot().status).toBe("connected");
    expect(conn.getSnapshot().sessions.length).toBe(1);
  });

  it("promotes status on LIST before READY", () => {
    expect(conn.getSnapshot().status).toBe("authenticating");
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    expect(conn.getSnapshot().status).toBe("connected");
    expect(conn.getSnapshot().ready).toBe(false);
  });

  it("does not look connected until LIST arrives", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    expect(conn.getSnapshot().status).toBe("authenticating");
    // Surface frames/other server activity can arrive before the terminal
    // list. They should not make the remote look connected while terminals
    // are still unknown.
    transport.push(
      new Uint8Array([
        0x20, // S2C_SURFACE_CREATED
        0x01,
        0x00, // surface_id
        0x00,
        0x00, // parent_id
        0x40,
        0x00, // width
        0x30,
        0x00, // height
        0x00,
        0x00, // title_len
        0x00,
        0x00, // app_id_len
      ]),
    );
    expect(conn.getSnapshot().status).toBe("authenticating");
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    expect(conn.getSnapshot().status).toBe("connected");
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
      "a",
      "b",
      "c",
    ]);
  });

  // --- Focus ---

  it("focusedSessionId is null for empty LIST", () => {
    transport.pushList([]);
    expect(conn.getSnapshot().focusedSessionId).toBeNull();
  });

  it("focusedSessionId auto-focuses first session for non-empty LIST", () => {
    transport.pushList([
      { ptyId: 5, tag: "a" },
      { ptyId: 6, tag: "b" },
    ]);
    const snap = conn.getSnapshot();
    const first = snap.sessions.find((s) => s.tag === "a");
    expect(snap.focusedSessionId).toBe(first?.id);
  });

  it("focusedSessionId moves to next active on CLOSED", () => {
    transport.pushList([
      { ptyId: 1, tag: "first" },
      { ptyId: 2, tag: "second" },
    ]);
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

  it("createSession with cwd sets features", () => {
    conn.createSession({ rows: 24, cols: 80, cwd: "/src/blit" });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    expect(msg[7]).toBe(CREATE2_HAS_CWD);
    const cwdLen = msg[10] | (msg[11] << 8);
    expect(cwdLen).toBe(9);
    expect(new TextDecoder().decode(msg.subarray(12, 21))).toBe("/src/blit");
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

  it("createSession via S2C_CREATED_N populates command immediately", async () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    const promise = conn.createSession({ rows: 24, cols: 80, command: "htop" });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    const nonce = msg[1] | (msg[2] << 8);
    transport.pushCreatedN(nonce, 7, "");
    const session = await promise;
    expect(session.command).toBe("htop");
  });

  it("createSession falls back to FIFO via S2C_CREATED", async () => {
    const promise = conn.createSession({ rows: 24, cols: 80, tag: "test" });
    transport.pushCreated(42, "test");
    const session = await promise;
    expect(session.tag).toBe("test");
  });

  it("createSession via S2C_CREATED FIFO populates command immediately", async () => {
    const promise = conn.createSession({ rows: 24, cols: 80, command: "vim" });
    transport.pushCreated(5, "");
    const session = await promise;
    expect(session.command).toBe("vim");
  });

  it("createSession rejects on disconnect", async () => {
    const promise = conn.createSession({ rows: 24, cols: 80 });
    transport.setStatus("disconnected");
    await expect(promise).rejects.toThrow(/disconnected/);
  });

  it("createSession omits WANT_STATUS when the server does not advertise it", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    conn.createSession({ rows: 24, cols: 80 });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    expect(msg[7] & CREATE2_WANT_STATUS).toBe(0);
  });

  it("createSession sets WANT_STATUS when the server advertises it", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE | FEATURE_CREATE_STATUS);
    conn.createSession({ rows: 24, cols: 80 });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    expect(msg[7] & CREATE2_WANT_STATUS).toBe(CREATE2_WANT_STATUS);
  });

  it("S2C_CREATE_FAILED rejects only the matching nonce", async () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE | FEATURE_CREATE_STATUS);
    const first = conn.createSession({ rows: 24, cols: 80, tag: "a" });
    const second = conn.createSession({ rows: 24, cols: 80, tag: "b" });
    const [msgA, msgB] = transport.sent.filter((m) => m[0] === C2S_CREATE2);
    transport.pushCreateFailed(
      msgA[1] | (msgA[2] << 8),
      STATUS_BUDGET,
      "pty cap reached",
    );
    await expect(first).rejects.toThrow(
      "Create failed: budget exhausted: pty cap reached",
    );
    transport.pushCreatedN(msgB[1] | (msgB[2] << 8), 9, "b");
    await expect(second).resolves.toMatchObject({ ptyId: 9, tag: "b" });
  });

  it("S2C_CREATE_FAILED with an empty detail omits the trailing text", async () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE | FEATURE_CREATE_STATUS);
    const promise = conn.createSession({ rows: 24, cols: 80 });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    transport.pushCreateFailed(msg[1] | (msg[2] << 8), STATUS_OTHER);
    await expect(promise).rejects.toThrow(/^Create failed: backend error$/);
  });

  it("S2C_CREATE_FAILED renders an unallocated status distinctly from OTHER", async () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE | FEATURE_CREATE_STATUS);
    const promise = conn.createSession({ rows: 24, cols: 80 });
    const msg = transport.sent.find((m) => m[0] === C2S_CREATE2)!;
    transport.pushCreateFailed(msg[1] | (msg[2] << 8), 200);
    await expect(promise).rejects.toThrow("Create failed: unknown status 200");
  });

  // --- copyRange ---

  it("copyRange resolves the copied text with the PTY's line count", async () => {
    transport.pushList([{ ptyId: 7, tag: "" }]);
    const session = conn.getSnapshot().sessions[0];
    const promise = conn.copyRange(session.id, 100, 0, 0, 0);
    const msg = transport.sent.find((m) => m[0] === C2S_COPY_RANGE)!;
    transport.pushText(msg[1] | (msg[2] << 8), 7, 4242, "one\ntwo");
    await expect(promise).resolves.toEqual({
      text: "one\ntwo",
      totalLines: 4242,
    });
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
    const sent = transport.sent
      .slice(before)
      .filter((msg) => msg[0] === C2S_RESIZE);
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

  // --- S2C_QUIT ---

  it("triggers immediate reconnect on S2C_QUIT", () => {
    expect(transport.reconnectCount).toBe(0);
    transport.pushQuit();
    expect(transport.reconnectCount).toBe(1);
  });

  it("clears sessions and sets ready=false on S2C_QUIT", () => {
    transport.pushCreated(1, "shell");
    transport.pushCreated(2, "vim");
    expect(conn.getSnapshot().sessions).toHaveLength(2);
    expect(conn.getSnapshot().sessions.every((s) => s.state !== "closed")).toBe(
      true,
    );

    transport.pushQuit();

    const snap = conn.getSnapshot();
    expect(snap.ready).toBe(false);
    expect(snap.focusedSessionId).toBeNull();
    // All sessions should be marked closed.
    for (const s of snap.sessions) {
      expect(s.state).toBe("closed");
    }
  });

  // --- S2C_HELLO ---

  it("closes transport on hello with version > PROTOCOL_VERSION", () => {
    transport.pushHello(2, FEATURE_CREATE_NONCE);
    expect(conn.getSnapshot().status).toBe("closed");
  });

  it("accepts hello with version 1", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    // S2C_HELLO proves the server is responsive, but terminals are not known
    // until S2C_LIST arrives. Keep the remote authenticating so it does not
    // appear connected while its terminal list is still missing.
    expect(conn.getSnapshot().status).toBe("authenticating");
    expect(conn.getSnapshot().ready).toBe(false);
  });

  it("exposes the server boot generation from hello", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE, 0x1234_5678_9abc_def0n);
    expect(conn.getSnapshot().bootGeneration).toBe(0x1234_5678_9abc_def0n);
  });

  it("accepts legacy hello without a boot generation", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE);
    expect(conn.getSnapshot().bootGeneration).toBeNull();
  });

  it("exposes the server version from hello", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE, 1n, "0.40.1");
    expect(conn.getSnapshot().serverVersion).toBe("0.40.1");
  });

  it("leaves the server version null for servers that omit it", () => {
    transport.pushHello(1, FEATURE_CREATE_NONCE, 1n);
    expect(conn.getSnapshot().serverVersion).toBeNull();
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
    const active = conn
      .getSnapshot()
      .sessions.filter((s) => s.state === "active");
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
    expect(conn.getSnapshot().sessions.find((s) => s.tag === "max")?.tag).toBe(
      "max",
    );
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

  it("sessions are closed on transport disconnect", () => {
    const { conn, transport } = createConnection();
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    transport.setStatus("disconnected");
    // Sessions should be dismissed so the UI doesn't show stale terminals
    // from a server that crashed without sending S2C_QUIT.
    expect(conn.getSnapshot().sessions.length).toBe(1);
    expect(conn.getSnapshot().sessions[0].state).toBe("closed");
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
    const live = s.filter((x) => x.state !== "closed");
    const closed = s.filter((x) => x.state === "closed");
    // "b" and "c" are live from the server's LIST.
    expect(live.length).toBe(2);
    expect(live.find((x) => x.tag === "b")?.state).toBe("active");
    expect(live.find((x) => x.tag === "c")?.state).toBe("active");
    // "a" was closed on disconnect and not in the reconnect LIST.
    expect(closed.some((x) => x.tag === "a")).toBe(true);
    // Title from before disconnect is preserved (session reuse by ptyId).
    expect(closed.find((x) => x.tag === "a")?.title).toBe("vim");
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
    transport.pushReady();
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
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
      { ptyId: 3, tag: "c" },
    ]);
    const s2 = conn.getSnapshot().sessions.find((s) => s.tag === "b")!;
    conn.focusSession(s2.id);
    transport.pushList([
      { ptyId: 2, tag: "b" },
      { ptyId: 3, tag: "c" },
    ]);
    const snap = conn.getSnapshot();
    const focused = snap.sessions.find((s) => s.id === snap.focusedSessionId);
    expect(focused?.tag).toBe("b");
  });

  it("focusedSessionId falls back when focused pty removed from LIST", () => {
    const { conn, transport } = createConnection();
    transport.pushList([
      { ptyId: 1, tag: "a" },
      { ptyId: 2, tag: "b" },
    ]);
    const s1 = conn.getSnapshot().sessions.find((s) => s.tag === "a")!;
    conn.focusSession(s1.id);
    transport.pushList([{ ptyId: 2, tag: "b" }]);
    const snap = conn.getSnapshot();
    const focused = snap.sessions.find((s) => s.id === snap.focusedSessionId);
    expect(focused?.tag).toBe("b");
  });

  // --- View size tracking ---

  it("removeView sends clearSessionSize when last view is removed", () => {
    const { conn, transport } = createConnection();
    transport.pushHello(1, FEATURE_RESIZE_BATCH);
    transport.pushCreated(1, "");
    const session = conn.getSnapshot().sessions[0];
    const viewId = conn.allocViewId();
    conn.setViewSize(session.id, viewId, 24, 80);
    const before = transport.sent.length;
    conn.removeView(session.id, viewId);
    const sent = transport.sent.slice(before);
    const resizes = sent.filter((m) => m[0] === C2S_RESIZE);
    expect(resizes).toHaveLength(1);
    const msg = resizes[0]!;
    expect(msg[1] | (msg[2] << 8)).toBe(1);
    // rows=0, cols=0 signals size constraint removal
    expect(msg[3] | (msg[4] << 8)).toBe(0);
    expect(msg[5] | (msg[6] << 8)).toBe(0);
  });

  it("removeView recalculates minimum when other views remain", () => {
    const { conn, transport } = createConnection();
    transport.pushHello(1, FEATURE_RESIZE_BATCH);
    transport.pushCreated(1, "");
    const session = conn.getSnapshot().sessions[0];
    const v1 = conn.allocViewId();
    const v2 = conn.allocViewId();
    conn.setViewSize(session.id, v1, 24, 80);
    conn.setViewSize(session.id, v2, 40, 120);
    const before = transport.sent.length;
    conn.removeView(session.id, v1);
    const sent = transport.sent.slice(before);
    const resizes = sent.filter((m) => m[0] === C2S_RESIZE);
    expect(resizes).toHaveLength(1);
    const msg = resizes[0]!;
    // Should send the remaining view's size (40x120), not a clear
    expect(msg[3] | (msg[4] << 8)).toBe(40);
    expect(msg[5] | (msg[6] << 8)).toBe(120);
  });
});

describe("BlitConnection git", () => {
  const u16 = (b: Uint8Array, at: number) => b[at] | (b[at + 1] << 8);
  const u32 = (b: Uint8Array, at: number) =>
    (b[at] | (b[at + 1] << 8) | (b[at + 2] << 16)) + b[at + 3] * 0x1000000;
  const oid = (fill: number) => new Uint8Array(32).fill(fill, 0, 20);

  // No TS server-side builder for S2C_GIT_REPO; assemble the success reply by
  // hand: [0x50][nonce:2][repo_id:2][status:1][oid_format:1][flags:1][wd][gd].
  function gitRepoReply(nonce: number, repoId: number): Uint8Array {
    const enc = new TextEncoder();
    const wd = enc.encode("/repo");
    const gd = enc.encode("/repo/.git");
    return new Uint8Array([
      S2C_GIT_REPO,
      nonce & 0xff,
      (nonce >> 8) & 0xff,
      repoId & 0xff,
      (repoId >> 8) & 0xff,
      GIT_STATUS_OK,
      GIT_OID_FORMAT_SHA1,
      0,
      wd.length & 0xff,
      (wd.length >> 8) & 0xff,
      ...wd,
      gd.length & 0xff,
      (gd.length >> 8) & 0xff,
      ...gd,
    ]);
  }

  async function openRepo(): Promise<{
    conn: BlitConnection;
    transport: MockTransport;
    repo: GitRepoHandle;
  }> {
    const { conn, transport } = createConnection();
    transport.pushHello(1, FEATURE_GIT);
    transport.pushList([]);
    const promise = conn.openRepo("/repo", {});
    const open = transport.sent.at(-1)!;
    expect(open[0]).toBe(C2S_GIT_OPEN);
    transport.push(gitRepoReply(u16(open, 1), 1));
    return { conn, transport, repo: await promise };
  }

  it("resolves a revspec to tips/hides", async () => {
    const { transport, repo } = await openRepo();
    const p = repo.resolve("main..dev");
    const req = transport.sent.at(-1)!;
    expect(req[0]).toBe(C2S_GIT_RESOLVE);
    transport.push(
      msgGitResolveResp(u16(req, 1), GIT_STATUS_OK, [oid(0xcc)], [oid(0xdd)]),
    );
    const res = await p;
    expect(res.tips.map((o) => o[0])).toEqual([0xcc]);
    expect(res.hides.map((o) => o[0])).toEqual([0xdd]);
  });

  it("delivers watched log pages, auto-acks, and unwatches on close", async () => {
    const { transport, repo } = await openRepo();
    const pages: GitLogPage[] = [];
    const sub = repo.watchLog("main", {}, (page) => pages.push(page));
    const watch = transport.sent.at(-1)!;
    expect(watch[0]).toBe(C2S_GIT_LOG_WATCH);
    const logId = u16(watch, 1);

    // First pushed page delivers and is acknowledged (update_id at offset 5).
    transport.push(
      msgGitLogPage(logId, 1, GIT_STATUS_OK, 0, [], new Uint8Array(0)),
    );
    expect(pages).toHaveLength(1);
    const ack = transport.sent.at(-1)!;
    expect(ack[0]).toBe(C2S_GIT_LOG_ACK);
    expect(u16(ack, 1)).toBe(logId);
    expect(u32(ack, 5)).toBe(1);

    // A later page (the ref moved) delivers again.
    transport.push(
      msgGitLogPage(logId, 2, GIT_STATUS_OK, 0, [], new Uint8Array(0)),
    );
    expect(pages).toHaveLength(2);

    // Closing unsubscribes and drops any further pages.
    sub.close();
    expect(transport.sent.at(-1)![0]).toBe(C2S_GIT_LOG_UNWATCH);
    transport.push(
      msgGitLogPage(logId, 3, GIT_STATUS_OK, 0, [], new Uint8Array(0)),
    );
    expect(pages).toHaveLength(2);
  });

  // A FROM_PTY open whose source session cannot be resolved used to fall back
  // to a plain path-based open: with the follow-terminal `path: ""` that is
  // `blit_git::open("")` — refused as "invalid path", leaving the dock's
  // commit log on "Loading…" for good. Refuse it here instead, so the caller
  // sees a real error rather than a silently rebased root.
  it("refuses a FROM_PTY open whose source session is unknown", async () => {
    const { conn, transport } = createConnection();
    transport.pushHello(1, FEATURE_GIT);
    transport.pushList([{ ptyId: 1, tag: "a" }]);
    const before = transport.sent.length;
    await expect(
      conn.openRepo("", { watch: true, fromSessionId: "test:404" }),
    ).rejects.toThrow(/source terminal/i);
    expect(transport.sent.slice(before)).toHaveLength(0);
  });
});

describe("BlitConnection fragment reassembly", () => {
  const fragment = (last: boolean, payload: Uint8Array) => {
    const m = new Uint8Array(2 + payload.length);
    m[0] = S2C_FRAGMENT;
    m[1] = last ? FRAGMENT_FLAG_LAST : 0;
    m.set(payload, 2);
    return m;
  };

  it("reassembles a fragmented message and clears the buffer", () => {
    const { conn, transport } = createConnection();
    transport.push(fragment(false, new Uint8Array([S2C_PING])));
    expect((conn as unknown as { fragmentBytes: number }).fragmentBytes).toBe(
      1,
    );
    transport.push(fragment(true, new Uint8Array([0x00])));
    expect((conn as unknown as { fragmentBytes: number }).fragmentBytes).toBe(
      0,
    );
  });

  // Without a ceiling, a peer that never sets FRAGMENT_FLAG_LAST grows the
  // buffer until the tab dies — and each chunk is a subarray pinning the
  // whole frame it arrived in, so the retained bytes are worse than the
  // payload alone. The Rust reader has always refused past this same bound.
  it("drops an unterminated fragment stream at the decompression ceiling", () => {
    const { conn, transport } = createConnection();
    const bytes = () =>
      (conn as unknown as { fragmentBytes: number }).fragmentBytes;

    const chunk = new Uint8Array(8 * 1024 * 1024);
    const parts = FS_MAX_DECOMPRESSED / chunk.length;
    for (let i = 0; i < parts; i++) transport.push(fragment(false, chunk));
    expect(bytes()).toBe(FS_MAX_DECOMPRESSED);

    // One byte more than the ceiling can hold: drop the partial rather than
    // keep growing.
    transport.push(fragment(false, new Uint8Array([0x00])));
    expect(bytes()).toBe(0);
  });
});

describe("BlitConnection surface subscriptions", () => {
  let transport: MockTransport;
  let conn: BlitConnection;

  beforeEach(() => {
    ({ conn, transport } = createConnection());
  });

  /** The size on the last SUBSCRIBE sent, or null if it was mediated. */
  function lastTarget(): { width: number; height: number } | null {
    const subs = transport.sent.filter((m) => m[0] === C2S_SURFACE_SUBSCRIBE);
    const msg = subs[subs.length - 1];
    if (!msg) throw new Error("no surface subscribe was sent");
    if (msg.length < 10) return null;
    const v = new DataView(msg.buffer, msg.byteOffset, msg.byteLength);
    return { width: v.getUint16(6, true), height: v.getUint16(8, true) };
  }

  const countSubscribes = () =>
    transport.sent.filter((m) => m[0] === C2S_SURFACE_SUBSCRIBE).length;

  it("asks for the size a lone scaled view wants", () => {
    conn.sendSurfaceSubscribe(1, conn.allocSurfaceViewId(), {
      width: 368,
      height: 523,
    });
    expect(lastTarget()).toEqual({ width: 368, height: 523 });
  });

  it("lets an unscaled view win over a scaled one", () => {
    // The full-size pane needs pixels no downscale can reconstruct, and the
    // thumbnail can always shrink what it is given.
    const thumb = conn.allocSurfaceViewId();
    const pane = conn.allocSurfaceViewId();
    conn.sendSurfaceSubscribe(1, thumb, { width: 368, height: 523 });
    conn.sendSurfaceSubscribe(1, pane, null);
    expect(lastTarget()).toBeNull();
  });

  it("takes the largest request when every view is scaled", () => {
    conn.sendSurfaceSubscribe(1, conn.allocSurfaceViewId(), {
      width: 368,
      height: 200,
    });
    conn.sendSurfaceSubscribe(1, conn.allocSurfaceViewId(), {
      width: 180,
      height: 523,
    });
    // Per axis — neither view is starved of the resolution it asked for.
    expect(lastTarget()).toEqual({ width: 368, height: 523 });
  });

  it("shrinks back to the thumbnail when the full-size view unmounts", () => {
    const thumb = conn.allocSurfaceViewId();
    const pane = conn.allocSurfaceViewId();
    conn.sendSurfaceSubscribe(1, thumb, { width: 368, height: 523 });
    conn.sendSurfaceSubscribe(1, pane, null);
    expect(lastTarget()).toBeNull();
    // A bare refcount could not do this: it knows one view left, not which.
    conn.sendSurfaceUnsubscribe(1, pane);
    expect(lastTarget()).toEqual({ width: 368, height: 523 });
  });

  it("re-derives when a view changes its own request", () => {
    const view = conn.allocSurfaceViewId();
    conn.sendSurfaceSubscribe(1, view, { width: 368, height: 523 });
    conn.setSurfaceViewTarget(1, view, { width: 184, height: 262 });
    expect(lastTarget()).toEqual({ width: 184, height: 262 });
  });

  it("does not resend when the derived request is unchanged", () => {
    // Every resubscribe costs the server an encoder rebuild and this client
    // a keyframe, so an unchanged derivation must stay off the wire.
    const view = conn.allocSurfaceViewId();
    conn.sendSurfaceSubscribe(1, view, { width: 368, height: 523 });
    const before = countSubscribes();
    conn.setSurfaceViewTarget(1, view, { width: 368, height: 523 });
    expect(countSubscribes()).toBe(before);
  });

  it("keeps surfaces independent", () => {
    conn.sendSurfaceSubscribe(1, conn.allocSurfaceViewId(), null);
    conn.sendSurfaceSubscribe(2, conn.allocSurfaceViewId(), {
      width: 320,
      height: 180,
    });
    expect(lastTarget()).toEqual({ width: 320, height: 180 });
  });
});

describe("BlitConnection surface view sizes", () => {
  let transport: MockTransport;
  let conn: BlitConnection;

  beforeEach(() => {
    ({ conn, transport } = createConnection());
  });

  /** Every C2S_SURFACE_RESIZE on the wire, decoded to (w, h, scale120). */
  function resizes(): { w: number; h: number; s: number }[] {
    return transport.sent
      .filter((m) => m[0] === C2S_SURFACE_RESIZE)
      .map((m) => {
        const v = new DataView(m.buffer, m.byteOffset, m.byteLength);
        return {
          w: v.getUint16(3, true),
          h: v.getUint16(5, true),
          s: v.getUint16(7, true),
        };
      });
  }

  it("puts a lone view's size on the wire", () => {
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    expect(resizes()).toEqual([{ w: 800, h: 600, s: 120 }]);
  });

  it("does not resend an unchanged size", () => {
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    expect(resizes()).toHaveLength(1);
  });

  it("survives a view handoff without unsizing the surface", () => {
    // The wire carries one size per (client, surface) and 0×0 means
    // "unset".  A pane moving between two UI locations mounts the new
    // view before the old one is disposed; the old view's withdrawal
    // must not wipe the size the new view just sent — that left the
    // server with no sizing client and the surface stuck until the
    // pane's box happened to change.
    conn.offerSurfaceViewSize(1, "old", 800, 600, 120);
    conn.offerSurfaceViewSize(1, "new", 800, 600, 120);
    conn.withdrawSurfaceViewSize(1, "old");
    expect(resizes()).toEqual([{ w: 800, h: 600, s: 120 }]);
  });

  it("re-sends the surviving offer when the winner withdraws", () => {
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    conn.offerSurfaceViewSize(1, "b", 1024, 768, 120);
    conn.withdrawSurfaceViewSize(1, "b");
    expect(resizes()).toEqual([
      { w: 800, h: 600, s: 120 },
      { w: 1024, h: 768, s: 120 },
      { w: 800, h: 600, s: 120 },
    ]);
  });

  it("unsets only when the last sized view withdraws", () => {
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    conn.withdrawSurfaceViewSize(1, "a");
    expect(resizes()).toEqual([
      { w: 800, h: 600, s: 120 },
      { w: 0, h: 0, s: 0 },
    ]);
  });

  it("ignores a withdrawal from a view that never offered", () => {
    conn.offerSurfaceViewSize(1, "a", 800, 600, 120);
    conn.withdrawSurfaceViewSize(1, "ghost");
    expect(resizes()).toHaveLength(1);
  });
});
