import { describe, it, expect } from "vitest";
import type { BlitWasmModule } from "../TerminalStore";
import { TerminalStore } from "../TerminalStore";
import { MockTransport } from "./mock-transport";
import { C2S_ACK, C2S_CLIENT_METRICS } from "../types";

class FakeTerminal {
  constructor(
    _rows: number,
    _cols: number,
    _cellPw: number,
    _cellPh: number,
  ) {}

  set_font_family(_fontFamily: string): void {}
  set_default_colors(
    _fgR: number,
    _fgG: number,
    _fgB: number,
    _bgR: number,
    _bgG: number,
    _bgB: number,
  ): void {}
  set_ansi_color(_idx: number, _r: number, _g: number, _b: number): void {}
  feed_compressed(_data: Uint8Array): void {}
  free(): void {}
}

const wasm = {
  Terminal: FakeTerminal,
} as unknown as BlitWasmModule;

describe("TerminalStore client metrics", () => {
  it("reports applied-frame backlog and clears it after render", async () => {
    const transport = new MockTransport();
    const store = new TerminalStore(transport, wasm);
    await Promise.resolve();

    transport.pushUpdate(7, new Uint8Array([1, 2, 3]));
    await Promise.resolve();

    expect(transport.sent[0]).toEqual(new Uint8Array([C2S_ACK]));

    const appliedMetrics = transport.sent.find(
      (msg) => msg[0] === C2S_CLIENT_METRICS,
    );
    expect(appliedMetrics).toBeTruthy();
    expect(
      (appliedMetrics![1] | (appliedMetrics![2] << 8)) >>> 0,
    ).toBe(1);
    expect(
      (appliedMetrics![3] | (appliedMetrics![4] << 8)) >>> 0,
    ).toBe(1);

    store.noteFrameRendered();
    await Promise.resolve();

    const clearedMetrics = transport.sent[transport.sent.length - 1];
    expect(clearedMetrics[0]).toBe(C2S_CLIENT_METRICS);
    expect((clearedMetrics[1] | (clearedMetrics[2] << 8)) >>> 0).toBe(0);
    expect((clearedMetrics[3] | (clearedMetrics[4] << 8)) >>> 0).toBe(0);

    store.dispose();
  });
});
