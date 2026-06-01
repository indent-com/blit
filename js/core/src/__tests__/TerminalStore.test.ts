import { afterEach, describe, it, expect } from "vitest";
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

function setNavigatorField(name: string, value: unknown): void {
  Object.defineProperty(navigator, name, {
    configurable: true,
    value,
  });
}

afterEach(() => {
  delete (navigator as Navigator & { gpu?: unknown }).gpu;
  delete (navigator as Navigator & { userAgent?: unknown }).userAgent;
  delete (navigator as Navigator & { platform?: unknown }).platform;
  delete (navigator as Navigator & { maxTouchPoints?: unknown }).maxTouchPoints;
});

describe("TerminalStore WebGPU probe", () => {
  it("probes WebGPU on iPadOS WebKit when navigator.gpu is present", () => {
    // iPad was previously force-disabled; we now let it use WebGPU like any
    // other platform (it falls back to WebGL2 if the probe fails).
    setNavigatorField("gpu", {});
    setNavigatorField(
      "userAgent",
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
    );
    setNavigatorField("platform", "MacIntel");
    setNavigatorField("maxTouchPoints", 5);

    const delegate: TerminalStoreDelegate = {
      send: () => {},
      getStatus: () => "disconnected",
    };
    const store = new TerminalStore(delegate, wasm);

    expect(
      (store as unknown as { webgpuProbe: Promise<void> | null }).webgpuProbe,
    ).not.toBeNull();

    store.destroy();
  });

  it("does not probe WebGPU when navigator.gpu is absent", () => {
    const delegate: TerminalStoreDelegate = {
      send: () => {},
      getStatus: () => "disconnected",
    };
    const store = new TerminalStore(delegate, wasm);

    expect(
      (store as unknown as { webgpuProbe: Promise<void> | null }).webgpuProbe,
    ).toBeNull();

    store.destroy();
  });
});

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
