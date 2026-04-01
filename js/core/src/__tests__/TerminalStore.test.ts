import { describe, it, expect } from "vitest";
import type { BlitWasmModule } from "../TerminalStore";
import { TerminalStore, type TerminalStoreDelegate } from "../TerminalStore";
import { MockTransport } from "./mock-transport";
import { C2S_ACK, C2S_CLIENT_METRICS } from "../types";

class FakeTerminal {
  constructor(_rows: number, _cols: number, _cellPw: number, _cellPh: number) {}

  set_font_family(_fontFamily: string): void {}
  set_font_size(_fontSize: number): void {}
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
    const delegate: TerminalStoreDelegate = {
      send: (data) => transport.send(data),
      getStatus: () => transport.status,
    };
    const store = new TerminalStore(delegate, wasm);

    // Simulate connected status
    store.handleStatusChange("connected");
    transport.sent = [];

    store.handleUpdate(7, new Uint8Array([1, 2, 3]));
    await Promise.resolve();

    const appliedMetrics = transport.sent.find(
      (msg) => msg[0] === C2S_CLIENT_METRICS,
    );
    expect(appliedMetrics).toBeTruthy();
    expect((appliedMetrics![1] | (appliedMetrics![2] << 8)) >>> 0).toBe(1);
    expect((appliedMetrics![3] | (appliedMetrics![4] << 8)) >>> 0).toBe(1);

    store.noteFrameRendered();
    await Promise.resolve();

    const acksAfterRender = transport.sent.filter((msg) => msg[0] === C2S_ACK);
    expect(acksAfterRender.length).toBeGreaterThan(0);

    const clearedMetrics = transport.sent
      .filter((msg) => msg[0] === C2S_CLIENT_METRICS)
      .pop()!;
    expect(clearedMetrics).toBeTruthy();
    expect((clearedMetrics[1] | (clearedMetrics[2] << 8)) >>> 0).toBe(0);
    expect((clearedMetrics[3] | (clearedMetrics[4] << 8)) >>> 0).toBe(0);

    store.destroy();
  });
});
