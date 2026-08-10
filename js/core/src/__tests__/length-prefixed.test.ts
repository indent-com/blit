import { describe, expect, it } from "vitest";
import { LengthPrefixedFrameDecoder } from "../transports/length-prefixed";

function frame(payload: number[]): Uint8Array {
  const bytes = new Uint8Array(4 + payload.length);
  bytes[0] = payload.length & 0xff;
  bytes[1] = (payload.length >> 8) & 0xff;
  bytes[2] = (payload.length >> 16) & 0xff;
  bytes[3] = (payload.length >> 24) & 0xff;
  bytes.set(payload, 4);
  return bytes;
}

function concat(...chunks: Uint8Array[]): Uint8Array {
  const result = new Uint8Array(
    chunks.reduce((length, chunk) => length + chunk.length, 0),
  );
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
}

describe("LengthPrefixedFrameDecoder", () => {
  it("delivers complete coalesced frames as zero-copy input views", () => {
    const input = concat(frame([1, 2, 3]), frame([4, 5]));
    const frames: number[][] = [];
    const inputBuffers: boolean[] = [];
    const decoder = new LengthPrefixedFrameDecoder(1024, (value) => {
      frames.push(Array.from(value));
      inputBuffers.push(value.buffer === input.buffer);
    });

    expect(decoder.push(input)).toBe(true);
    expect(frames).toEqual([
      [1, 2, 3],
      [4, 5],
    ]);
    expect(inputBuffers).toEqual([true, true]);
  });

  it("reassembles a header and payload split across arbitrary chunks", () => {
    const input = concat(frame([1, 2, 3, 4]), frame([5, 6]));
    const frames: number[][] = [];
    const decoder = new LengthPrefixedFrameDecoder(1024, (value) => {
      frames.push(Array.from(value));
    });

    expect(decoder.push(input.subarray(0, 2))).toBe(true);
    expect(decoder.push(input.subarray(2, 7))).toBe(true);
    expect(decoder.push(input.subarray(7))).toBe(true);
    expect(frames).toEqual([
      [1, 2, 3, 4],
      [5, 6],
    ]);
  });

  it("rejects negative and oversized lengths", () => {
    const negative = new Uint8Array([0, 0, 0, 0x80]);
    const oversized = new Uint8Array([5, 0, 0, 0]);
    const decoder = new LengthPrefixedFrameDecoder(4, () => {});

    expect(decoder.push(negative)).toBe(false);
    expect(new LengthPrefixedFrameDecoder(4, () => {}).push(oversized)).toBe(
      false,
    );
  });
});
