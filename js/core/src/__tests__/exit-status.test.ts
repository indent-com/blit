import { describe, it, expect } from "vitest";

import { BlitConnection } from "../BlitConnection";
import {
  EXIT_STATUS_UNKNOWN,
  exitCodeFromStatus,
  formatExitStatus,
} from "../exit-status";
import type { BlitWasmModule } from "../TerminalStore";
import { MockTransport } from "./mock-transport";

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

function createConnection() {
  const transport = new MockTransport();
  const conn = new BlitConnection({
    id: "test",
    transport,
    wasm,
    autoConnect: false,
  });
  return { conn, transport };
}

describe("exitCodeFromStatus", () => {
  it("maps unknown status to 1", () => {
    expect(exitCodeFromStatus(EXIT_STATUS_UNKNOWN)).toBe(1);
  });

  it("passes through normal exit codes", () => {
    expect(exitCodeFromStatus(0)).toBe(0);
    expect(exitCodeFromStatus(2)).toBe(2);
    expect(exitCodeFromStatus(127)).toBe(127);
  });

  it("maps signals to 128 + signal", () => {
    expect(exitCodeFromStatus(-9)).toBe(137); // SIGKILL
    expect(exitCodeFromStatus(-15)).toBe(143); // SIGTERM
  });
});

describe("formatExitStatus", () => {
  it("renders the canonical strings", () => {
    expect(formatExitStatus(EXIT_STATUS_UNKNOWN)).toBe("exited");
    expect(formatExitStatus(0)).toBe("exited(0)");
    expect(formatExitStatus(3)).toBe("exited(3)");
    expect(formatExitStatus(-2)).toBe("signal(2)");
  });
});

describe("BlitConnection S2C_EXITED", () => {
  it("records exitStatus from a normal exit", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(7, "job");
    transport.pushExited(7, 0);
    const session = conn.getSnapshot().sessions[0]!;
    expect(session.state).toBe("exited");
    expect(session.exitStatus).toBe(0);
    expect(exitCodeFromStatus(session.exitStatus!)).toBe(0);
  });

  it("records a non-zero exit code", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(7);
    transport.pushExited(7, 3);
    const session = conn.getSnapshot().sessions[0]!;
    expect(session.exitStatus).toBe(3);
    expect(exitCodeFromStatus(session.exitStatus!)).toBe(3);
  });

  it("records a negative (signalled) status", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(7);
    transport.pushExited(7, -9);
    const session = conn.getSnapshot().sessions[0]!;
    expect(session.exitStatus).toBe(-9);
    expect(exitCodeFromStatus(session.exitStatus!)).toBe(137);
  });

  it("is null while the session is running", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(7);
    expect(conn.getSnapshot().sessions[0]!.exitStatus).toBeNull();
  });

  it("falls back to EXIT_STATUS_UNKNOWN for a short frame", () => {
    const { conn, transport } = createConnection();
    transport.pushCreated(7);
    transport.pushExitedRaw(7); // no status bytes
    const session = conn.getSnapshot().sessions[0]!;
    expect(session.state).toBe("exited");
    expect(session.exitStatus).toBe(EXIT_STATUS_UNKNOWN);
  });
});
