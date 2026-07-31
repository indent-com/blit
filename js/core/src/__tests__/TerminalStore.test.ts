import { afterEach, describe, it, expect } from "vitest";
import type { BlitWasmModule } from "../TerminalStore";
import { TerminalStore, type TerminalStoreDelegate } from "../TerminalStore";
import type { GlRenderer } from "../gl-renderer";
import { MockTransport } from "./mock-transport";
import { C2S_ACK, C2S_CLIENT_METRICS, C2S_SUBSCRIBE } from "../types";

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

describe("TerminalStore GPU loss recovery", () => {
  type Internals = {
    sharedRenderer: GlRenderer | null;
    sharedCanvas: HTMLCanvasElement | null;
    webgpuRenderer: GlRenderer | null;
    webgpuCanvas: HTMLCanvasElement | null;
    handleWebGpuLost(): void;
  };

  function fakeRenderer() {
    const r = {
      supported: true,
      disposeCount: 0,
      dispose() {
        r.disposeCount++;
        r.supported = false;
      },
    };
    return r;
  }

  const asRenderer = (r: ReturnType<typeof fakeRenderer>) =>
    r as unknown as GlRenderer;

  function storeWithTerminal(): {
    store: TerminalStore;
    dirty: number[];
  } {
    const store = new TerminalStore(
      { send: () => {}, getStatus: () => "disconnected" },
      wasm,
    );
    // A terminal must exist for the repaint notification to have a target.
    store.handleUpdate(5, new Uint8Array([1]));
    const dirty: number[] = [];
    store.addDirtyListener((id) => dirty.push(id));
    return { store, dirty };
  }

  it("drops the dead device and its canvas, then repaints", () => {
    const { store, dirty } = storeWithTerminal();
    const internals = store as unknown as Internals;
    const gpu = fakeRenderer();
    const canvas = document.createElement("canvas");
    internals.webgpuRenderer = asRenderer(gpu);
    internals.webgpuCanvas = canvas;
    internals.sharedRenderer = asRenderer(gpu);
    internals.sharedCanvas = canvas;

    internals.handleWebGpuLost();

    expect(internals.sharedRenderer).toBeNull();
    // The canvas has to go with it: getContext("webgl2") on a canvas already
    // configured for WebGPU returns null, which would take the WebGL2 *and*
    // Canvas2D fallbacks down and leave getSharedRenderer returning null.
    expect(internals.sharedCanvas).toBeNull();
    // And the dead device must not be promotable again.
    expect(internals.webgpuRenderer).toBeNull();
    expect(gpu.disposeCount).toBe(1);
    // Rendering is event-driven, so recovery has to include a repaint or an
    // idle pane stays blank until its next output.
    expect(dirty).toContain(5);

    store.destroy();
  });

  it("keeps a healthy fallback when the device dies before promotion", () => {
    const { store } = storeWithTerminal();
    const internals = store as unknown as Internals;
    const gl = fakeRenderer();
    const gpu = fakeRenderer();
    const glCanvas = document.createElement("canvas");
    internals.sharedRenderer = asRenderer(gl);
    internals.sharedCanvas = glCanvas;
    internals.webgpuRenderer = asRenderer(gpu);
    internals.webgpuCanvas = document.createElement("canvas");

    internals.handleWebGpuLost();

    expect(internals.webgpuRenderer).toBeNull();
    expect(internals.sharedRenderer).toBe(asRenderer(gl));
    expect(internals.sharedCanvas).toBe(glCanvas);
    expect(gl.disposeCount).toBe(0);

    store.destroy();
  });
});

describe("TerminalStore frames arriving before WASM", () => {
  /** Records what was fed so a dropped frame is visible as a missing entry. */
  const fed: Uint8Array[] = [];
  class RecordingTerminal extends FakeTerminal {
    override feed_compressed(data: Uint8Array): void {
      fed.push(data);
    }
  }
  const lateWasm = {
    Terminal: RecordingTerminal,
  } as unknown as BlitWasmModule;

  it("queues them and applies them in order once it loads", async () => {
    fed.length = 0;
    let resolveWasm: (mod: BlitWasmModule) => void = () => {};
    const pending = new Promise<BlitWasmModule>((resolve) => {
      resolveWasm = resolve;
    });
    const transport = new MockTransport();
    const store = new TerminalStore(
      {
        send: (data) => transport.send(data),
        getStatus: () => transport.status,
      },
      pending,
    );

    const dirty: number[] = [];
    store.addDirtyListener((ptyId) => dirty.push(ptyId));

    // The server encodes each frame as a delta against what it believes we
    // hold and never resends, so dropping either of these would desync the
    // grid until a re-subscribe.
    store.handleUpdate(3, new Uint8Array([1]));
    store.handleUpdate(3, new Uint8Array([2]));
    expect(fed).toEqual([]);
    expect(store.getTerminal(3)).toBeNull();

    resolveWasm(lateWasm);
    await Promise.resolve();
    await Promise.resolve();

    expect(fed.map((f) => f[0])).toEqual([1, 2]);
    expect(store.getTerminal(3)).not.toBeNull();
    // And the surfaces are told, since doRender drops frames outright while
    // wasmMemory() is null and never retries on its own.
    expect(dirty).toContain(3);

    store.destroy();
  });

  it("re-subscribes instead of growing the queue without limit", async () => {
    fed.length = 0;
    const pending = new Promise<BlitWasmModule>(() => {});
    const transport = new MockTransport();
    const store = new TerminalStore(
      {
        send: (data) => transport.send(data),
        getStatus: () => transport.status,
      },
      pending,
    );
    store.handleStatusChange("connected");
    store.setDesiredSubscriptions(new Set([4]));
    transport.sent = [];

    const warn = console.warn;
    console.warn = () => {};
    try {
      for (let i = 0; i < 600; i++) {
        store.handleUpdate(4, new Uint8Array([i & 0xff]));
      }
    } finally {
      console.warn = warn;
    }

    // A gap in a delta stream is unrecoverable, so the queue is dropped and a
    // fresh subscribe asked for — the server then encodes a full frame against
    // an empty basis.
    expect(store.getDebugStats().totalPendingFrames).toBeLessThan(600);
    expect(transport.sent.some((msg) => msg[0] === C2S_SUBSCRIBE)).toBe(true);

    store.destroy();
  });
});
