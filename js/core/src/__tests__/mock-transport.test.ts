import { describe, it, expect, vi } from "vitest";
import { MockTransport } from "./mock-transport";
import { S2C_CREATED, S2C_CLOSED, S2C_LIST, S2C_TITLE } from "../types";

describe("MockTransport", () => {
  it("starts as connected", () => {
    const t = new MockTransport();
    expect(t.status).toBe("connected");
  });

  it("send captures data", () => {
    const t = new MockTransport();
    t.send(new Uint8Array([1, 2, 3]));
    expect(t.sent.length).toBe(1);
    expect(Array.from(t.sent[0])).toEqual([1, 2, 3]);
  });

  it("close sets disconnected", () => {
    const t = new MockTransport();
    const cb = vi.fn();
    t.addEventListener("statuschange", cb);
    t.close();
    expect(t.status).toBe("disconnected");
    expect(cb).toHaveBeenCalledWith("disconnected");
  });

  it("push delivers to message listeners", () => {
    const t = new MockTransport();
    const cb = vi.fn();
    t.addEventListener("message", cb);
    t.push(new Uint8Array([0xaa]));
    expect(cb).toHaveBeenCalledTimes(1);
    const buf = cb.mock.calls[0][0] as ArrayBuffer;
    expect(new Uint8Array(buf)).toEqual(new Uint8Array([0xaa]));
  });

  it("removeEventListener stops delivery", () => {
    const t = new MockTransport();
    const cb = vi.fn();
    t.addEventListener("message", cb);
    t.removeEventListener("message", cb);
    t.push(new Uint8Array([0xaa]));
    expect(cb).not.toHaveBeenCalled();
  });

  it("multiple listeners all receive messages", () => {
    const t = new MockTransport();
    const cb1 = vi.fn();
    const cb2 = vi.fn();
    t.addEventListener("message", cb1);
    t.addEventListener("message", cb2);
    t.push(new Uint8Array([0xaa]));
    expect(cb1).toHaveBeenCalledTimes(1);
    expect(cb2).toHaveBeenCalledTimes(1);
  });

  describe("pushCreated", () => {
    it("builds correct wire format without tag", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushCreated(5);
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_CREATED);
      expect(bytes[1] | (bytes[2] << 8)).toBe(5);
      expect(bytes.length).toBe(3);
    });

    it("builds correct wire format with tag", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushCreated(1, "hello");
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_CREATED);
      expect(bytes[1] | (bytes[2] << 8)).toBe(1);
      expect(new TextDecoder().decode(bytes.subarray(3))).toBe("hello");
    });
  });

  describe("pushClosed", () => {
    it("builds correct wire format", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushClosed(0x0102);
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_CLOSED);
      expect(bytes[1]).toBe(0x02);
      expect(bytes[2]).toBe(0x01);
    });
  });

  describe("pushList", () => {
    it("empty list", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushList([]);
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_LIST);
      expect(bytes[1] | (bytes[2] << 8)).toBe(0);
      expect(bytes.length).toBe(3);
    });

    it("entries with tags", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushList([{ ptyId: 1, tag: "ab" }, { ptyId: 2 }]);
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_LIST);
      expect(bytes[1] | (bytes[2] << 8)).toBe(2);

      let off = 3;
      expect(bytes[off] | (bytes[off + 1] << 8)).toBe(1);
      expect(bytes[off + 2] | (bytes[off + 3] << 8)).toBe(2);
      expect(new TextDecoder().decode(bytes.subarray(off + 4, off + 6))).toBe(
        "ab",
      );
      off += 6;
      expect(bytes[off] | (bytes[off + 1] << 8)).toBe(0);
      off += 2;

      expect(bytes[off] | (bytes[off + 1] << 8)).toBe(2);
      expect(bytes[off + 2] | (bytes[off + 3] << 8)).toBe(0);
      off += 4;
      expect(bytes[off] | (bytes[off + 1] << 8)).toBe(0);
    });
  });

  describe("pushTitle", () => {
    it("builds correct wire format", () => {
      const t = new MockTransport();
      const msgs: ArrayBuffer[] = [];
      t.addEventListener("message", (d) => msgs.push(d));
      t.pushTitle(7, "vim");
      const bytes = new Uint8Array(msgs[0]);
      expect(bytes[0]).toBe(S2C_TITLE);
      expect(bytes[1] | (bytes[2] << 8)).toBe(7);
      expect(new TextDecoder().decode(bytes.subarray(3))).toBe("vim");
    });
  });
});
