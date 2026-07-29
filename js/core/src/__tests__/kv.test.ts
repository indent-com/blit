import { describe, it, expect } from "vitest";
import {
  buildKvOpenMessage,
  buildKvStopMessage,
  buildKvAckMessage,
  buildKvPutMessage,
  buildKvFetchMessage,
  buildKvUpdateMessage,
  parseKvOpenedMessage,
  parseKvDoneMessage,
  parseKvValueMessage,
  parseKvUpdateMessage,
  encodeKvRecords,
  decodeKvRecords,
  kvKeyValid,
  KvMirror,
  KV_MAX_KEY,
  KV_STATUS_OK,
  KV_STATUS_CONFLICT,
  KV_UPDATE_SNAPSHOT_END,
  KV_PUT_DELETE,
  S2C_KV_OPENED,
  S2C_KV_DONE,
  S2C_KV_VALUE,
  C2S_KV_PUT,
} from "../kv";
import { fsCompressLiteral } from "../fs";

describe("kv wire format", () => {
  it("KV_OPEN bytes match the Rust fixture", () => {
    // Locked in crates/remote/src/kv.rs kv_open_roundtrip_and_bytes.
    expect(Array.from(buildKvOpenMessage(7, 0, 4096, "editor/"))).toEqual([
      0x70, 0x07, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x07, 0x00, 0x65, 0x64,
      0x69, 0x74, 0x6f, 0x72, 0x2f,
    ]);
  });

  it("KV_STOP / KV_ACK layouts", () => {
    expect(Array.from(buildKvStopMessage(3))).toEqual([0x71, 3, 0]);
    expect(Array.from(buildKvAckMessage(3, 99))).toEqual([
      0x72, 3, 0, 99, 0, 0, 0,
    ]);
  });

  it("KV_FETCH layout", () => {
    expect(Array.from(buildKvFetchMessage(5, "k"))).toEqual([
      0x74, 5, 0, 1, 0, 0x6b,
    ]);
  });

  it("KV_PUT roundtrips through the record layout", () => {
    const msg = buildKvPutMessage({
      nonce: 21,
      flags: KV_PUT_DELETE,
      base: (1n << 100n) | 42n,
      key: "roots",
      value: new Uint8Array(0),
    });
    expect(msg[0]).toBe(C2S_KV_PUT);
    const v = new DataView(msg.buffer);
    expect(v.getUint16(1, true)).toBe(21);
    expect(msg[3]).toBe(KV_PUT_DELETE);
    // base as two LE u64s, low word first.
    expect(v.getBigUint64(4, true)).toBe(42n);
    expect(v.getBigUint64(12, true)).toBe(1n << 36n);
    expect(v.getUint16(20, true)).toBe(5);
  });

  it("KV_OPENED / KV_DONE / KV_VALUE parse", () => {
    const opened = new Uint8Array([0x70, 9, 0, 2, 0, KV_STATUS_OK, 0, 0]);
    expect(opened[0]).toBe(S2C_KV_OPENED);
    expect(parseKvOpenedMessage(opened)).toEqual({
      nonce: 9,
      kvId: 2,
      status: KV_STATUS_OK,
      detail: "",
    });

    const done = new Uint8Array(28);
    done[0] = S2C_KV_DONE;
    const dv = new DataView(done.buffer);
    dv.setUint16(1, 4, true);
    done[3] = KV_STATUS_CONFLICT;
    dv.setBigUint64(4, 77n, true);
    dv.setBigUint64(20, 123456n, true);
    expect(parseKvDoneMessage(done)).toEqual({
      nonce: 4,
      status: KV_STATUS_CONFLICT,
      hash: 77n,
      mtimeNs: 123456n,
    });

    const payload = new TextEncoder().encode("payload");
    const compressed = fsCompressLiteral(payload);
    const value = new Uint8Array(20 + compressed.length);
    value[0] = S2C_KV_VALUE;
    const vv = new DataView(value.buffer);
    vv.setUint16(1, 6, true);
    value[3] = KV_STATUS_OK;
    vv.setBigUint64(4, 88n, true);
    value.set(compressed, 20);
    const parsed = parseKvValueMessage(value);
    expect(parsed?.nonce).toBe(6);
    expect(parsed?.hash).toBe(88n);
    expect(new TextDecoder().decode(parsed!.data)).toBe("payload");
  });

  it("records roundtrip and unknown kinds are skipped", () => {
    const records = encodeKvRecords([
      {
        kind: "upsert",
        key: "editor/open//a.rs",
        hash: 11n,
        size: 2,
        mtimeNs: 5n,
        value: new TextEncoder().encode("{}"),
      },
      { kind: "delete", key: "roots" },
    ]);
    // Splice in an unknown record kind (0x7f) with a 3-byte body.
    const unknown = new Uint8Array([4, 0, 0, 0, 0x7f, 1, 2, 3]);
    const buf = new Uint8Array(unknown.length + records.length);
    buf.set(unknown, 0);
    buf.set(records, unknown.length);
    const decoded = decodeKvRecords(buf);
    expect(decoded).toHaveLength(2);
    expect(decoded[0].kind).toBe("upsert");
    expect(decoded[1]).toEqual({ kind: "delete", key: "roots" });
  });

  it("mirror applies snapshot and live updates", () => {
    const mirror = new KvMirror();
    const snap = buildKvUpdateMessage(1, 10, KV_UPDATE_SNAPSHOT_END, [
      {
        kind: "upsert",
        key: "editor/open//a.rs",
        hash: 11n,
        size: 2,
        mtimeNs: 5n,
        value: new TextEncoder().encode("{}"),
      },
      {
        kind: "upsert",
        key: "editor/buf//a.rs",
        hash: 12n,
        size: 9999999,
        mtimeNs: 6n,
        value: null, // over inline_max: metadata only
      },
    ]);
    expect(mirror.applyUpdate(snap)).toBe(10);
    expect(mirror.snapshotDone).toBe(true);
    expect(mirror.live.size).toBe(2);
    expect(mirror.live.get("editor/buf//a.rs")?.value).toBeNull();

    const del = buildKvUpdateMessage(1, 11, 0, [
      { kind: "delete", key: "editor/open//a.rs" },
    ]);
    expect(mirror.applyUpdate(del)).toBe(11);
    expect(mirror.live.size).toBe(1);

    expect(parseKvUpdateMessage(snap)?.records).toHaveLength(2);
  });

  it("key validity", () => {
    expect(kvKeyValid("roots")).toBe(true);
    expect(kvKeyValid("editor/buf//x/y.rs")).toBe(true);
    expect(kvKeyValid("")).toBe(false);
    expect(kvKeyValid("k".repeat(KV_MAX_KEY + 1))).toBe(false);
    expect(kvKeyValid("a\0b")).toBe(false);
  });
});
