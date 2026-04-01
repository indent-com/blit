import { describe, it, expect, beforeEach } from "vitest";
import { BlitConnection } from "../BlitConnection";
import { MockTransport } from "./mock-transport";
import type { BlitWasmModule } from "../TerminalStore";
import {
  S2C_UPDATE,
  S2C_CREATED,
  S2C_CLOSED,
  S2C_LIST,
  S2C_TITLE,
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

function createConnection(transport: MockTransport) {
  return new BlitConnection({
    id: "test",
    transport,
    wasm,
    autoConnect: false,
  });
}

/**
 * Tests that verify the wire format parsing matches the blit protocol spec.
 * These test raw byte arrays, not the MockTransport helpers.
 */
describe("wire format parsing", () => {
  let transport: MockTransport;
  let conn: BlitConnection;

  beforeEach(() => {
    transport = new MockTransport();
    conn = createConnection(transport);
  });

  describe("S2C_UPDATE", () => {
    it("parses pty_id and creates terminal", () => {
      transport.pushCreated(0x0103, "test");
      const sessionId = conn.getSnapshot().sessions[0].id;
      transport.push(new Uint8Array([S2C_UPDATE, 0x03, 0x01, 0xde, 0xad]));
      expect(conn.getTerminal(sessionId)).not.toBeNull();
    });

    it("empty payload is valid", () => {
      transport.pushCreated(0, "test");
      const sessionId = conn.getSnapshot().sessions[0].id;
      transport.push(new Uint8Array([S2C_UPDATE, 0x00, 0x00]));
      expect(conn.getTerminal(sessionId)).not.toBeNull();
    });

    it("rejects 2-byte message", () => {
      transport.push(new Uint8Array([S2C_UPDATE, 0x00]));
      // No terminal should be created
      expect(conn.getTerminal(0)).toBeNull();
    });
  });

  describe("S2C_CREATED", () => {
    it("parses pty_id and tag", () => {
      // pty_id=0x00FF, tag="hi"
      transport.push(new Uint8Array([S2C_CREATED, 0xff, 0x00, 0x68, 0x69]));
      const s = conn.getSnapshot().sessions;
      expect(s[0].tag).toBe("hi");
    });

    it("parses without tag (just pty_id)", () => {
      transport.push(new Uint8Array([S2C_CREATED, 0x01, 0x00]));
      expect(conn.getSnapshot().sessions[0].tag).toBe("");
    });

    it("handles multi-byte UTF-8 tag", () => {
      // "é" is 0xC3 0xA9 in UTF-8
      transport.push(new Uint8Array([S2C_CREATED, 0x01, 0x00, 0xc3, 0xa9]));
      expect(conn.getSnapshot().sessions[0].tag).toBe("é");
    });
  });

  describe("S2C_CLOSED", () => {
    it("parses pty_id", () => {
      transport.pushCreated(7, "");
      transport.push(new Uint8Array([S2C_CLOSED, 0x07, 0x00]));
      expect(conn.getSnapshot().sessions[0].state).toBe("closed");
    });

    it("handles high pty_id", () => {
      transport.pushCreated(65535, "");
      transport.push(new Uint8Array([S2C_CLOSED, 0xff, 0xff]));
      expect(conn.getSnapshot().sessions[0].state).toBe("closed");
    });
  });

  describe("S2C_LIST", () => {
    it("parses multiple entries with tags", () => {
      // count=2
      // entry 1: pty_id=1, tag_len=2, tag="ab", cmd_len=0
      // entry 2: pty_id=2, tag_len=0, cmd_len=0
      transport.push(
        new Uint8Array([
          S2C_LIST,
          0x02,
          0x00,
          0x01,
          0x00,
          0x02,
          0x00,
          0x61,
          0x62,
          0x00,
          0x00,
          0x02,
          0x00,
          0x00,
          0x00,
          0x00,
          0x00,
        ]),
      );
      const s = conn.getSnapshot().sessions;
      expect(s.length).toBe(2);
      expect(s[0].tag).toBe("ab");
      expect(s[1].tag).toBe("");
    });

    it("parses empty list", () => {
      transport.push(new Uint8Array([S2C_LIST, 0x00, 0x00]));
      expect(conn.getSnapshot().sessions.length).toBe(0);
      expect(conn.getSnapshot().ready).toBe(true);
    });

    it("handles truncated list gracefully", () => {
      // count=2 but only 1 entry fits
      transport.push(
        new Uint8Array([
          S2C_LIST,
          0x02,
          0x00,
          0x01,
          0x00,
          0x00,
          0x00,
          0x00,
          0x00,
          // second entry missing
        ]),
      );
      const s = conn.getSnapshot().sessions;
      expect(s.length).toBe(1);
      expect(s[0].tag).toBe("");
    });

    it("handles long tags", () => {
      const tag = "x".repeat(300);
      const tagBytes = new TextEncoder().encode(tag);
      const msg = new Uint8Array(3 + 4 + tagBytes.length + 2);
      msg[0] = S2C_LIST;
      msg[1] = 1;
      msg[2] = 0; // count=1
      msg[3] = 0x05;
      msg[4] = 0x00; // pty_id=5
      msg[5] = tagBytes.length & 0xff;
      msg[6] = (tagBytes.length >> 8) & 0xff;
      msg.set(tagBytes, 7);
      msg[7 + tagBytes.length] = 0x00; // cmd_len low
      msg[7 + tagBytes.length + 1] = 0x00; // cmd_len high
      transport.push(msg);
      expect(conn.getSnapshot().sessions[0].tag).toBe(tag);
    });
  });

  describe("S2C_TITLE", () => {
    it("parses pty_id and title", () => {
      transport.pushCreated(3, "");
      const titleBytes = new TextEncoder().encode("my-shell");
      const msg = new Uint8Array(3 + titleBytes.length);
      msg[0] = S2C_TITLE;
      msg[1] = 0x03;
      msg[2] = 0x00;
      msg.set(titleBytes, 3);
      transport.push(msg);
      expect(conn.getSnapshot().sessions[0].title).toBe("my-shell");
    });

    it("handles empty title", () => {
      transport.pushCreated(1, "");
      transport.push(new Uint8Array([S2C_TITLE, 0x01, 0x00]));
      expect(conn.getSnapshot().sessions[0].title).toBe("");
    });
  });

  describe("unknown message types", () => {
    it("does not crash on unknown type", () => {
      // Type 0xFF is unknown
      transport.push(new Uint8Array([0xff, 0x01, 0x02, 0x03]));
      expect(conn.getSnapshot().sessions.length).toBe(0);
    });
  });

  describe("message ordering", () => {
    it("processes multiple messages in order", () => {
      transport.pushCreated(1, "a");
      const snap1 = conn.getSnapshot();
      expect(snap1.sessions[0].tag).toBe("a");

      transport.pushTitle(1, "vim");
      const snap2 = conn.getSnapshot();
      expect(snap2.sessions[0].title).toBe("vim");

      transport.pushClosed(1);
      const snap3 = conn.getSnapshot();
      expect(snap3.sessions[0].state).toBe("closed");
    });
  });
});
