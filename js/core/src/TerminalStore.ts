import type { Terminal } from "@blit-sh/browser";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import type { ConnectionStatus, TerminalPalette } from "./types";
import {
  buildAckMessage,
  buildClientMetricsMessage,
  buildDisplayRateMessage,
  buildSubscribeMessage,
  buildUnsubscribeMessage,
} from "./protocol";
import {
  createGlRenderer,
  createCanvas2dRenderer,
  type GlRenderer,
} from "./gl-renderer";
import { createWebGpuRenderer } from "./webgpu-renderer";

const DISPLAY_FPS_SAMPLE_COUNT = 120;

/**
 * Infer refresh from rAF intervals without assuming a table of display modes.
 *
 * Browser timestamps are quantized: a 240 Hz clock can alternate between
 * 4.1 and 4.2 ms.  Taking the median selects one bucket (4.2 -> 238 Hz),
 * while averaging recovers the underlying 4.166... ms period. Reject Tukey
 * outliers first so isolated missed callbacks and compensating intervals do
 * not skew the result without changing the ratio of the 4.1/4.2 ms buckets.
 */
export function estimateDisplayFps(intervals: readonly number[]): number {
  const sorted = [...intervals]
    .filter((dt) => Number.isFinite(dt) && dt > 0)
    .sort((a, b) => a - b);
  if (sorted.length === 0) return 0;

  const q1 = sorted[Math.floor((sorted.length - 1) * 0.25)];
  const q3 = sorted[Math.floor((sorted.length - 1) * 0.75)];
  const iqr = q3 - q1;
  const median = sorted[Math.floor((sorted.length - 1) * 0.5)];
  const low = iqr > Number.EPSILON ? q1 - 1.5 * iqr : median * 0.75;
  const high = iqr > Number.EPSILON ? q3 + 1.5 * iqr : median * 1.25;
  const accepted = sorted.filter((dt) => dt >= low && dt <= high);
  let total = 0;
  for (const dt of accepted) total += dt;
  const mean = total / accepted.length;
  return 1_000 / mean;
}

export type BlitWasmModule = typeof import("@blit-sh/browser");

export type TerminalDirtyListener = (ptyId: number) => void;

export interface TerminalStoreDelegate {
  send(data: Uint8Array): void;
  getStatus(): ConnectionStatus;
  log?(msg: string): void;
}

export class TerminalStore {
  /** Hidden documents retain low-rate liveness without spending a display
   *  refresh worth of terminal and compositor work. */
  private static readonly HIDDEN_DISPLAY_FPS = 4;
  private mod: BlitWasmModule | null = null;
  private terminals = new Map<number, Terminal>();
  private staleTerminals = new Map<number, Terminal>();
  private retainCount = new Map<number, number>();
  private pendingFree = new Set<number>();
  private subscribed = new Set<number>();
  private desired = new Set<number>();
  private readonly delegate: TerminalStoreDelegate;
  private dirtyListeners = new Set<TerminalDirtyListener>();
  private leadPtyId: number | null = null;
  private fontFamily = DEFAULT_FONT;
  private fontSize =
    DEFAULT_FONT_SIZE *
    (typeof devicePixelRatio !== "undefined" ? devicePixelRatio : 1);
  private cellPw = 1;
  private cellPh = 1;
  private palette: TerminalPalette | null = null;
  private disposed = false;
  private ready = false;
  private readyListeners = new Set<() => void>();
  /** Incremented every time any terminal's cell metrics are set, so renderers can detect stale state. */
  metricsGeneration = 0;
  private sharedRenderer: GlRenderer | null = null;
  private sharedCanvas: HTMLCanvasElement | null = null;
  private webgpuProbe: Promise<void> | null = null;
  private webgpuRenderer: GlRenderer | null = null;
  private webgpuCanvas: HTMLCanvasElement | null = null;
  private displayFps = 0;
  /** Small display-rate changes are commonly measurement noise, not a mode
   * change. Require the same nearby result repeatedly so a steady clock does
   * not alternate between adjacent integer rates every ten seconds. */
  private pendingDisplayFps = 0;
  private pendingDisplayFpsCount = 0;
  private static readonly DISPLAY_FPS_CONFIRMATIONS = 3;
  private static readonly IMMEDIATE_DISPLAY_FPS_DROP_RATIO = 0.9;
  private static readonly RAF_PROBE_MIN_SAMPLES = 20;
  private static readonly RAF_PROBE_DURATION_MS = 500;
  private rafHandle = 0;
  private rafProbeTimer: ReturnType<typeof setInterval> | null = null;
  private visibilityHandler: (() => void) | null = null;
  private rafPrev = 0;
  private rafProbeStartedAt = 0;
  private rafSamples: number[] = [];
  private pendingAppliedFrames = 0;
  private ackAheadFrames = 0;
  private applyMsX10 = 0;
  private metricsFlushQueued = false;
  private metricsHeartbeat: ReturnType<typeof setInterval> | null = null;
  private pendingAcks = 0;
  /** Queued compressed payloads per PTY, drained in the rAF callback. */
  private pendingFrames = new Map<number, Uint8Array[]>();

  constructor(
    delegate: TerminalStoreDelegate,
    wasm: BlitWasmModule | Promise<BlitWasmModule>,
  ) {
    this.delegate = delegate;
    this.startRafProbe();
    this.armRafProbe();
    this.probeWebGpu();

    if (wasm instanceof Promise) {
      wasm
        .then((mod) => {
          if (this.disposed) return;
          this.mod = mod;
          this.ready = true;
          // Replay whatever arrived while the module was loading, then repaint:
          // `BlitTerminalSurface.doRender` drops a frame outright while
          // `wasmMemory()` is null and never retries, so surfaces that asked
          // for a frame during the load window are still showing nothing.
          this.drainQueuedFrames();
          this.notifyAllDirty();
          for (const l of this.readyListeners) l();
        })
        .catch((err) => {
          console.error("blit: failed to load WASM module:", err);
        });
    } else {
      this.mod = wasm;
      this.ready = true;
    }
  }

  /** Fire-and-forget WebGPU probe. If it succeeds, the next getSharedRenderer
   *  call will pick it up. If it fails, we silently fall through to WebGL2. */
  private probeWebGpu(): void {
    if (typeof navigator === "undefined" || !navigator.gpu) return;
    const canvas = document.createElement("canvas");
    this.webgpuProbe = createWebGpuRenderer(canvas, () =>
      this.handleWebGpuLost(),
    )
      .then((r) => {
        if (this.disposed) {
          r?.dispose();
          return;
        }
        if (r) {
          this.webgpuCanvas = canvas;
          this.webgpuRenderer = r;
          // If the shared renderer was already initialised with a WebGL2 /
          // Canvas2D fallback, replace it now so the next frame uses WebGPU.
          if (this.sharedRenderer && this.sharedRenderer !== r) {
            this.sharedRenderer.dispose();
            this.sharedRenderer = r;
            this.sharedCanvas = canvas;
            // Surfaces cache the renderer and only re-fetch once `supported`
            // goes false (see gl-renderer), so the swap is picked up on a
            // surface's *next* render — and nothing here was scheduling one.
            // An idle pane therefore kept displaying its last pre-swap
            // composite until output arrived or the cursor blink fired 530ms
            // later, and a read-only surface (no blink) kept it for good.
            this.notifyAllDirty();
          }
        }
      })
      .catch(() => {})
      .finally(() => {
        this.webgpuProbe = null;
      });
  }

  private nowMs(): number {
    if (
      typeof performance !== "undefined" &&
      typeof performance.now === "function"
    ) {
      return performance.now();
    }
    return Date.now();
  }

  private resetClientMetrics(): void {
    this.pendingAppliedFrames = 0;
    this.ackAheadFrames = 0;
    this.applyMsX10 = 0;
    this.metricsFlushQueued = false;
  }

  private queueClientMetricsFlush(): void {
    if (this.metricsFlushQueued) return;
    this.metricsFlushQueued = true;
    const flush = () => {
      this.metricsFlushQueued = false;
      this.flushClientMetrics();
    };
    if (typeof queueMicrotask === "function") {
      queueMicrotask(flush);
    } else {
      void Promise.resolve().then(flush);
    }
  }

  private startMetricsHeartbeat(): void {
    this.stopMetricsHeartbeat();
    // Send metrics every 250ms so the server always has fresh backlog info,
    // even when no renders are happening (which would otherwise cause a
    // deadlock: server stops sending because backlog is high, client never
    // renders because no new frames arrive, backlog never clears).
    this.metricsHeartbeat = setInterval(() => this.flushClientMetrics(), 250);
  }

  private stopMetricsHeartbeat(): void {
    if (this.metricsHeartbeat !== null) {
      clearInterval(this.metricsHeartbeat);
      this.metricsHeartbeat = null;
    }
  }

  private flushClientMetrics(): void {
    if (this.disposed || this.delegate.getStatus() !== "connected") return;
    this.delegate.send(
      buildClientMetricsMessage(
        Math.min(this.pendingAppliedFrames, 0xffff),
        Math.min(this.ackAheadFrames, 0xffff),
        Math.min(this.applyMsX10, 0xffff),
      ),
    );
  }

  private noteAppliedFrame(applyMs: number): void {
    this.pendingAppliedFrames = Math.min(this.pendingAppliedFrames + 1, 0xffff);
    this.ackAheadFrames = Math.min(this.ackAheadFrames + 1, 0xffff);
    const sampleX10 = Math.min(Math.round(applyMs * 10), 0xffff);
    this.applyMsX10 =
      this.applyMsX10 > 0
        ? Math.round(this.applyMsX10 * 0.8 + sampleX10 * 0.2)
        : sampleX10;
    this.queueClientMetricsFlush();
  }

  isReady(): boolean {
    return this.ready;
  }

  private _wasmMem: WebAssembly.Memory | null = null;

  /** Get the WASM linear memory for zero-copy typed array views. */
  wasmMemory(): WebAssembly.Memory | null {
    if (this._wasmMem) return this._wasmMem;
    if (!this.mod) return null;
    const m = this.mod as Record<string, unknown>;
    if (typeof m.wasm_memory === "function") {
      this._wasmMem = (m.wasm_memory as () => WebAssembly.Memory)();
      return this._wasmMem;
    }
    return null;
  }

  onReady(listener: () => void): () => void {
    if (this.ready) {
      listener();
      return () => {};
    }
    this.readyListeners.add(listener);
    return () => this.readyListeners.delete(listener);
  }

  private createTerminal(): Terminal {
    const t = new this.mod!.Terminal(24, 80, this.cellPw, this.cellPh);
    if (typeof t.set_font_family === "function")
      t.set_font_family(this.fontFamily);
    if (typeof t.set_font_size === "function") t.set_font_size(this.fontSize);
    if (this.palette) {
      t.set_default_colors(...this.palette.fg, ...this.palette.bg);
      for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...this.palette.ansi[i]);
    }
    return t;
  }

  handleUpdate(ptyId: number, payload: Uint8Array): void {
    this.pendingAcks++;

    // No WASM yet: queue, don't drop. Frames are deltas against the state the
    // server believes we hold (see the ops encoder in crates/remote), and it
    // never resends — so a discarded frame desyncs the grid until the next
    // re-subscribe. On a cold load the discarded one is typically the full
    // frame that follows subscribe, which is what leaves a pane blank or
    // half-painted with cells that no later update ever corrects.
    if (!this.mod) {
      this.queueFrame(ptyId, payload);
      return;
    }

    const applyStart = this.nowMs();
    let terminal = this.terminals.get(ptyId);
    if (!terminal) {
      terminal = this.createTerminal();
      this.terminals.set(ptyId, terminal);
      const stale = this.staleTerminals.get(ptyId);
      if (stale) {
        this.staleTerminals.delete(ptyId);
        stale.free();
      }
    }
    terminal.feed_compressed(payload);
    this.noteAppliedFrame(this.nowMs() - applyStart);
    for (const listener of this.dirtyListeners) listener(ptyId);

    // ACK immediately after applying the frame.  The server's congestion
    // window stalls permanently if ACKs are delayed until noteFrameRendered()
    // because that only fires when a BlitTerminalSurface actually renders —
    // and a terminal for a *different* PTY having a dirty listener doesn't
    // help the PTY that received this update.  The server uses separate
    // metrics (browser_backlog, apply_ms) for pacing; the ACK just prevents
    // the inflight window from filling up.
    //
    // Only drain ACKs here — do NOT reset pendingAppliedFrames / ackAheadFrames.
    // Those counters reflect unrendered backlog and are cleared when the UI
    // actually paints via noteFrameRendered().
    while (this.pendingAcks > 0 && this.delegate.getStatus() === "connected") {
      this.pendingAcks--;
      this.delegate.send(buildAckMessage());
    }
  }

  handleStatusChange(status: ConnectionStatus): void {
    if (status === "connected") {
      this.resetClientMetrics();
      this.flushClientMetrics();
      this.resync();
      this.startMetricsHeartbeat();
    } else if (status === "disconnected" || status === "error") {
      this.subscribed.clear();
      this.resetClientMetrics();
      this.pendingAcks = 0;
      this.stopMetricsHeartbeat();
    }
  }

  getTerminal(ptyId: number): Terminal | null {
    return this.terminals.get(ptyId) ?? this.staleTerminals.get(ptyId) ?? null;
  }

  setLead(ptyId: number | null): void {
    this.leadPtyId = ptyId;
  }

  setFontFamily(fontFamily: string): void {
    this.fontFamily = fontFamily;
  }

  setFontSize(fontSize: number): void {
    this.fontSize = fontSize;
  }

  /** Resolve the canvas a caller should composite FROM via drawImage. Every
   *  backend renders into its own canvas which the caller drawImages
   *  synchronously right after render(), so we hand back the canvas directly. */
  private compositeCanvas(
    _renderer: GlRenderer,
    canvas: HTMLCanvasElement,
  ): HTMLCanvasElement {
    return canvas;
  }

  /**
   * Throw away the shared renderer after a GPU context or device loss, so the
   * next {@link getSharedRenderer} builds a replacement.
   *
   * The canvas goes with it, and that is the point: a canvas keeps the context
   * it was first given for life. A canvas whose WebGL2 context was lost hands
   * back that same dead context from `getContext("webgl2")`, and a canvas
   * configured for WebGPU refuses `getContext("webgl2")` altogether — which
   * would have taken the WebGL2 *and* Canvas2D fallbacks down with it and left
   * `getSharedRenderer` returning null forever. Only a fresh element rebinds.
   *
   * Repainting is part of the recovery: rendering is event-driven, so without
   * this an idle pane would sit blank until its next output.
   */
  private discardSharedRenderer(): void {
    if (this.disposed) return;
    const dead = this.sharedRenderer;
    if (!dead) return;
    if (dead === this.webgpuRenderer) {
      // Don't let getSharedRenderer promote the dead device straight back in.
      this.webgpuRenderer = null;
      this.webgpuCanvas = null;
    }
    this.sharedRenderer = null;
    this.sharedCanvas = null;
    dead.dispose();
    // If the GPU is gone for good the rebuild lands on Canvas2D, which has no
    // context to lose — so this converges instead of looping.
    this.notifyAllDirty();
  }

  /**
   * The WebGPU device died. Drop it as a candidate so
   * {@link getSharedRenderer} can't promote it back, and rebuild only if it was
   * the renderer actually in use — a device lost before the probe promoted it
   * should cost a healthy WebGL2 fallback nothing.
   */
  private handleWebGpuLost(): void {
    if (this.disposed) return;
    const wasShared =
      this.sharedRenderer !== null &&
      this.sharedRenderer === this.webgpuRenderer;
    this.webgpuRenderer = null;
    this.webgpuCanvas = null;
    if (wasShared) this.discardSharedRenderer();
  }

  /** Get a shared renderer for all surfaces. Prefers WebGPU (async probe),
   *  falls back to WebGL2, then Canvas 2D. */
  getSharedRenderer(): {
    renderer: GlRenderer;
    canvas: HTMLCanvasElement;
  } | null {
    if (this.sharedRenderer?.supported) {
      return {
        renderer: this.sharedRenderer,
        canvas: this.compositeCanvas(this.sharedRenderer, this.sharedCanvas!),
      };
    }
    // Use WebGPU renderer if the async probe has completed.
    if (this.webgpuRenderer?.supported && this.webgpuCanvas) {
      this.sharedRenderer = this.webgpuRenderer;
      this.sharedCanvas = this.webgpuCanvas;
      return {
        renderer: this.sharedRenderer,
        canvas: this.compositeCanvas(this.sharedRenderer, this.sharedCanvas),
      };
    }
    // Synchronous fallback: WebGL2 → Canvas 2D.
    if (!this.sharedCanvas) {
      this.sharedCanvas = document.createElement("canvas");
    }
    this.sharedRenderer = createGlRenderer(this.sharedCanvas, () =>
      this.discardSharedRenderer(),
    );
    if (!this.sharedRenderer.supported) {
      this.sharedRenderer = createCanvas2dRenderer(this.sharedCanvas);
    }
    if (!this.sharedRenderer.supported) return null;
    return {
      renderer: this.sharedRenderer,
      canvas: this.compositeCanvas(this.sharedRenderer, this.sharedCanvas),
    };
  }

  /** Upper bound on frames held per PTY while the WASM module loads. Well
   *  above what a sub-second module fetch can accumulate; the cap only exists
   *  so a module that never resolves can't grow the queue without limit. */
  private static readonly MAX_QUEUED_FRAMES = 512;

  /**
   * Hold a frame that can't be decoded yet.
   *
   * On overflow the queue is dropped and the PTY re-subscribed rather than
   * replayed: a gap in a delta stream is unrecoverable, but a fresh subscribe
   * makes the server encode against an empty basis and send a full frame.
   */
  private queueFrame(ptyId: number, payload: Uint8Array): void {
    let queue = this.pendingFrames.get(ptyId);
    if (!queue) {
      queue = [];
      this.pendingFrames.set(ptyId, queue);
    }
    if (queue.length >= TerminalStore.MAX_QUEUED_FRAMES) {
      console.warn(
        `blit: dropping ${queue.length} queued frames for pty ${ptyId} — re-subscribing`,
      );
      this.pendingFrames.delete(ptyId);
      if (this.subscribed.has(ptyId)) {
        this.subscribed.delete(ptyId);
        this.syncSubscriptions();
      }
      return;
    }
    queue.push(new Uint8Array(payload));
  }

  /**
   * Apply everything queued by {@link queueFrame}, in arrival order, creating
   * the terminals the frames belong to. Callers repaint afterwards.
   */
  private drainQueuedFrames(): void {
    if (!this.mod || this.pendingFrames.size === 0) return;
    const applyStart = this.nowMs();
    for (const [ptyId, queue] of this.pendingFrames) {
      if (queue.length === 0) continue;
      let terminal = this.terminals.get(ptyId);
      if (!terminal) {
        terminal = this.createTerminal();
        this.terminals.set(ptyId, terminal);
        const stale = this.staleTerminals.get(ptyId);
        if (stale) {
          this.staleTerminals.delete(ptyId);
          stale.free();
        }
      }
      for (const payload of queue) terminal.feed_compressed(payload);
    }
    this.pendingFrames.clear();
    this.noteAppliedFrame(this.nowMs() - applyStart);
  }

  /**
   * Force every surface to re-prepare and repaint.
   *
   * The dirty listeners are the existing "this terminal's content changed"
   * path, and a surface's callback already sets `contentDirty` and schedules a
   * frame. That is exactly what a late WASM load or a renderer swap needs even
   * though no cell changed — and reusing it beats a second notification
   * mechanism that would have to stay in step with the first.
   */
  private notifyAllDirty(): void {
    for (const ptyId of this.terminals.keys()) {
      for (const listener of this.dirtyListeners) listener(ptyId);
    }
  }

  /** Mark the latest applied terminal state as painted to the screen. */
  noteFrameRendered(): void {
    // Send ACKs now that the frames have been rendered — not before.
    // This keeps ACKs in sync with actual rendering, so the server's
    // RTT measurement reflects render time and backlog stays accurate.
    while (this.pendingAcks > 0 && this.delegate.getStatus() === "connected") {
      this.pendingAcks--;
      this.delegate.send(buildAckMessage());
    }
    this.pendingAppliedFrames = 0;
    this.ackAheadFrames = 0;
    this.queueClientMetricsFlush();
  }

  getDebugStats(leadPtyId?: number | null): {
    displayFps: number;
    rendererBackend: string;
    pendingApplied: number;
    ackAhead: number;
    applyMs: number;
    mouseMode: number;
    mouseEncoding: number;
    terminals: number;
    staleTerminals: number;
    subscribed: number;
    pendingFrameQueues: number;
    totalPendingFrames: number;
  } {
    let totalPending = 0;
    for (const q of this.pendingFrames.values()) totalPending += q.length;
    const lead = leadPtyId != null ? this.terminals.get(leadPtyId) : null;
    return {
      displayFps: this.displayFps,
      rendererBackend: this.sharedRenderer?.backend ?? "none",
      pendingApplied: this.pendingAppliedFrames,
      ackAhead: this.ackAheadFrames,
      applyMs: this.applyMsX10 / 10,
      mouseMode: lead ? lead.mouse_mode() : 0,
      mouseEncoding: lead ? lead.mouse_encoding() : 0,
      terminals: this.terminals.size,
      staleTerminals: this.staleTerminals.size,
      subscribed: this.subscribed.size,
      pendingFrameQueues: this.pendingFrames.size,
      totalPendingFrames: totalPending,
    };
  }

  invalidateAtlas(): void {
    for (const t of this.terminals.values()) {
      t.invalidate_render_cache();
    }
  }

  setPalette(palette: TerminalPalette): void {
    this.palette = palette;
    for (const t of this.terminals.values()) {
      t.set_default_colors(...palette.fg, ...palette.bg);
      for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...palette.ansi[i]);
    }
  }

  setCellSize(pw: number, ph: number): void {
    this.cellPw = pw;
    this.cellPh = ph;
  }

  getCellSize(): { pw: number; ph: number } {
    return { pw: this.cellPw, ph: this.cellPh };
  }

  setDesiredSubscriptions(ptyIds: Set<number>): void {
    this.desired = new Set(ptyIds);
    this.syncSubscriptions();
    // When all terminal subscriptions are cleared, reset the browser metrics
    // so stale terminal-only counters (pendingAppliedFrames, ackAheadFrames)
    // don't poison the server's shared flow-control window — which would
    // block surface (compositor) frame delivery on that connection.
    if (this.desired.size === 0 && this.subscribed.size === 0) {
      this.resetClientMetrics();
      this.flushClientMetrics();
    }
  }

  /**
   * Get the current retain count for a PTY.
   */
  getRetainCount(ptyId: number): number {
    return this.retainCount.get(ptyId) ?? 0;
  }

  retain(ptyId: number): void {
    this.retainCount.set(ptyId, (this.retainCount.get(ptyId) ?? 0) + 1);
  }

  release(ptyId: number): void {
    const count = (this.retainCount.get(ptyId) ?? 1) - 1;
    if (count <= 0) {
      this.retainCount.delete(ptyId);
      if (this.pendingFree.has(ptyId)) {
        this.pendingFree.delete(ptyId);
        this.doFree(ptyId);
      }
    } else {
      this.retainCount.set(ptyId, count);
    }
  }

  freeTerminal(ptyId: number): void {
    if ((this.retainCount.get(ptyId) ?? 0) > 0) {
      this.pendingFree.add(ptyId);
    } else {
      this.doFree(ptyId);
    }
  }

  private doFree(ptyId: number): void {
    this.pendingFrames.delete(ptyId);
    const t = this.terminals.get(ptyId);
    if (t) {
      t.free();
      this.terminals.delete(ptyId);
    }
    const stale = this.staleTerminals.get(ptyId);
    if (stale) {
      stale.free();
      this.staleTerminals.delete(ptyId);
    }
    this.subscribed.delete(ptyId);
  }

  addDirtyListener(listener: TerminalDirtyListener): () => void {
    this.dirtyListeners.add(listener);
    return () => this.dirtyListeners.delete(listener);
  }

  private syncSubscriptions(): void {
    if (this.delegate.getStatus() !== "connected") return;
    for (const id of this.desired) {
      if (!this.subscribed.has(id)) {
        this.subscribed.add(id);
        // Keep existing terminals across re-subscriptions so scrollback
        // is preserved.  The server's fresh frame updates the visible
        // state on the same WASM object.
        this.delegate.send(buildSubscribeMessage(id));
      }
    }
    for (const id of this.subscribed) {
      if (!this.desired.has(id)) {
        this.subscribed.delete(id);
        this.delegate.send(buildUnsubscribeMessage(id));
        // Don't free the terminal — BlitTerminal may still hold a ref.
        // It will be freed on PTY close or store dispose.
      }
    }
  }

  private sendDisplayFps(): void {
    if (this.displayFps > 0 && this.delegate.getStatus() === "connected") {
      this.delegate.send(buildDisplayRateMessage(this.displayFps));
    }
  }

  /** Apply one completed rAF probe. Mode-sized changes take effect
   * immediately; adjacent integer results need confirmation because they are
   * the expected rounding noise around a stable physical refresh rate. */
  private acceptDisplayFps(fps: number): boolean {
    if (fps <= 0) return false;
    if (
      this.displayFps === 0 ||
      fps >= this.displayFps + 2 ||
      fps <=
        this.displayFps * TerminalStore.IMMEDIATE_DISPLAY_FPS_DROP_RATIO
    ) {
      const changed = fps !== this.displayFps;
      this.displayFps = fps;
      this.pendingDisplayFps = 0;
      this.pendingDisplayFpsCount = 0;
      return changed;
    }
    if (fps === this.displayFps) {
      this.pendingDisplayFps = 0;
      this.pendingDisplayFpsCount = 0;
      return false;
    }
    if (fps !== this.pendingDisplayFps) {
      this.pendingDisplayFps = fps;
      this.pendingDisplayFpsCount = 1;
      return false;
    }
    this.pendingDisplayFpsCount++;
    if (
      this.pendingDisplayFpsCount < TerminalStore.DISPLAY_FPS_CONFIRMATIONS
    )
      return false;
    this.displayFps = fps;
    this.pendingDisplayFps = 0;
    this.pendingDisplayFpsCount = 0;
    return true;
  }

  private startRafProbe(): void {
    if (
      this.rafHandle ||
      typeof requestAnimationFrame === "undefined" ||
      (typeof document !== "undefined" &&
        document.visibilityState === "hidden")
    )
      return;
    this.rafPrev = 0;
    this.rafProbeStartedAt = 0;
    this.rafSamples = [];
    const measure = (ts: number) => {
      if (this.disposed) return;
      if (this.rafProbeStartedAt === 0) this.rafProbeStartedAt = ts;
      if (this.rafPrev > 0) {
        const dt = ts - this.rafPrev;
        if (dt > 0) {
          this.rafSamples.push(dt);
          if (
            this.rafSamples.length >= TerminalStore.RAF_PROBE_MIN_SAMPLES &&
            ts - this.rafProbeStartedAt >=
              TerminalStore.RAF_PROBE_DURATION_MS
          ) {
            this.rafSamples.sort((a, b) => a - b);
            // Browser timestamps are commonly quantized to 0.1 ms. At
            // 240 Hz the true 4.1667 ms period therefore alternates between
            // 4.1 and 4.2 ms; taking the median alone selects 4.2 ms and
            // systematically reports 238 Hz. Average the middle 80% instead:
            // quantization cancels across the window, while a missed rAF or
            // one unusually early callback is trimmed away.
            const trim = Math.floor(this.rafSamples.length * 0.1);
            const stableSamples = this.rafSamples.slice(
              trim,
              this.rafSamples.length - trim,
            );
            const mean =
              stableSamples.reduce((sum, sample) => sum + sample, 0) /
              stableSamples.length;
            const fps = Math.round(1_000 / mean);
            this.rafSamples = [];
            if (this.acceptDisplayFps(fps)) {
              this.sendDisplayFps();
            }
            // Established, so stop. `armRafProbe` samples another short
            // window periodically; keeping this callback alive every frame
            // for the life of the page showed up in profiles and denied the
            // browser idle frames for style coalescing.
            this.stopRafProbe();
            return;
          }
        }
      }
      this.rafPrev = ts;
      this.rafHandle = requestAnimationFrame(measure);
    };
    this.rafHandle = requestAnimationFrame(measure);
  }

  /**
   * Re-measure the display rate when it may have changed: returning to a
   * visible tab, which is also when a window has plausibly been dragged to
   * a monitor with a different refresh rate, and every ten seconds while
   * visible. The periodic sample lets a startup probe taken during a busy
   * burst recover; each probe stops after a 500 ms measurement window.
   */
  private armRafProbe(): void {
    if (
      typeof document === "undefined" ||
      typeof requestAnimationFrame === "undefined"
    )
      return;
    this.visibilityHandler = () => {
      if (document.visibilityState === "visible") {
        // Restore the last measured rate immediately; the fresh probe takes
        // about 500 ms and should refine cadence, not hold it at 4 Hz.
        this.sendDisplayFps();
        this.startRafProbe();
      } else if (this.delegate.getStatus() === "connected") {
        this.stopRafProbe();
        this.delegate.send(
          buildDisplayRateMessage(TerminalStore.HIDDEN_DISPLAY_FPS),
        );
      }
    };
    document.addEventListener("visibilitychange", this.visibilityHandler);
    this.rafProbeTimer = setInterval(() => {
      if (document.visibilityState === "visible") this.startRafProbe();
    }, 10_000);
  }

  private stopRafProbe(): void {
    if (this.rafHandle) {
      cancelAnimationFrame(this.rafHandle);
      this.rafHandle = 0;
    }
  }

  private resync(): void {
    if (
      typeof document !== "undefined" &&
      document.visibilityState === "hidden" &&
      this.delegate.getStatus() === "connected"
    ) {
      this.delegate.send(
        buildDisplayRateMessage(TerminalStore.HIDDEN_DISPLAY_FPS),
      );
    } else {
      this.sendDisplayFps();
    }
    this.subscribed.clear();
    this.syncSubscriptions();
  }

  /** Permanently destroy the store — free all WASM terminals and GL resources. */
  destroy(): void {
    this.disposed = true;
    this.stopRafProbe();
    if (this.rafProbeTimer !== null) {
      clearInterval(this.rafProbeTimer);
      this.rafProbeTimer = null;
    }
    if (this.visibilityHandler) {
      document.removeEventListener("visibilitychange", this.visibilityHandler);
      this.visibilityHandler = null;
    }
    this.stopMetricsHeartbeat();
    for (const t of this.terminals.values()) t.free();
    this.terminals.clear();
    for (const t of this.staleTerminals.values()) t.free();
    this.staleTerminals.clear();
    this.subscribed.clear();
    this.dirtyListeners.clear();
    this.readyListeners.clear();
    this.sharedRenderer?.dispose();
    this.sharedRenderer = null;
    this.sharedCanvas = null;
    // If the WebGPU renderer was created but never promoted to sharedRenderer,
    // dispose it separately.
    if (this.webgpuRenderer && this.webgpuRenderer !== this.sharedRenderer) {
      this.webgpuRenderer.dispose();
    }
    this.webgpuRenderer = null;
    this.webgpuCanvas = null;
  }
}
