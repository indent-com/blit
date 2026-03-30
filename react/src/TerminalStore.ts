import type { Terminal } from "blit-browser";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import type { ConnectionStatus, TerminalPalette } from "./types";
import {
  buildAckMessage,
  buildClientMetricsMessage,
  buildDisplayRateMessage,
  buildSubscribeMessage,
  buildUnsubscribeMessage,
} from "./protocol";
import { createGlRenderer, type GlRenderer } from "./gl-renderer";

export type BlitWasmModule = typeof import("blit-browser");

export type TerminalDirtyListener = (ptyId: number) => void;

export interface TerminalStoreDelegate {
  send(data: Uint8Array): void;
  getStatus(): ConnectionStatus;
}

export class TerminalStore {
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
  private frozenPtys = new Set<number>();
  /** Incremented every time any terminal's cell metrics are set, so renderers can detect stale state. */
  metricsGeneration = 0;
  private frozenBuffers = new Map<number, Uint8Array[]>();
  private sharedRenderer: GlRenderer | null = null;
  private sharedCanvas: HTMLCanvasElement | null = null;
  private displayFps = 0;
  private rafHandle = 0;
  private rafPrev = 0;
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

    if (wasm instanceof Promise) {
      wasm
        .then((mod) => {
          if (this.disposed) return;
          this.mod = mod;
          this.ready = true;
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
    // Buffer frames for frozen PTYs (e.g. during selection).
    if (this.frozenPtys.has(ptyId)) {
      let buf = this.frozenBuffers.get(ptyId);
      if (!buf) {
        buf = [];
        this.frozenBuffers.set(ptyId, buf);
      }
      buf.push(new Uint8Array(payload));
      return;
    }

    const applyStart = this.nowMs();
    let terminal = this.terminals.get(ptyId);
    if (!terminal) {
      if (!this.mod) return;
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

  /** Get a shared GL renderer for readOnly (preview) terminals. */
  getSharedRenderer(): {
    renderer: GlRenderer;
    canvas: HTMLCanvasElement;
  } | null {
    if (this.sharedRenderer?.supported) {
      return { renderer: this.sharedRenderer, canvas: this.sharedCanvas! };
    }
    if (!this.sharedCanvas) {
      this.sharedCanvas = document.createElement("canvas");
    }
    this.sharedRenderer = createGlRenderer(this.sharedCanvas);
    if (!this.sharedRenderer.supported) return null;
    return { renderer: this.sharedRenderer, canvas: this.sharedCanvas };
  }

  /**
   * Drain queued compressed frames for a PTY into the WASM terminal.
   * Called at the start of the rAF callback so decode + render happen
   * in one JS turn, avoiding budget fragmentation.
   * Returns true if any frames were applied.
   */
  drainPending(ptyId: number): boolean {
    const q = this.pendingFrames.get(ptyId);
    if (!q || q.length === 0) return false;
    const t = this.terminals.get(ptyId);
    if (!t) {
      q.length = 0;
      return false;
    }
    const applyStart = this.nowMs();
    for (const payload of q) {
      t.feed_compressed(payload);
    }
    q.length = 0;
    this.noteAppliedFrame(this.nowMs() - applyStart);
    return true;
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
    pendingApplied: number;
    ackAhead: number;
    applyMs: number;
    mouseMode: number;
    mouseEncoding: number;
    terminals: number;
    staleTerminals: number;
    subscribed: number;
    frozenPtys: number;
    pendingFrameQueues: number;
    totalPendingFrames: number;
  } {
    let totalPending = 0;
    for (const q of this.pendingFrames.values()) totalPending += q.length;
    const lead = leadPtyId != null ? this.terminals.get(leadPtyId) : null;
    return {
      displayFps: this.displayFps,
      pendingApplied: this.pendingAppliedFrames,
      ackAhead: this.ackAheadFrames,
      applyMs: this.applyMsX10 / 10,
      mouseMode: lead ? lead.mouse_mode() : 0,
      mouseEncoding: lead ? lead.mouse_encoding() : 0,
      terminals: this.terminals.size,
      staleTerminals: this.staleTerminals.size,
      subscribed: this.subscribed.size,
      frozenPtys: this.frozenPtys.size,
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

  /** Freeze a PTY: buffer incoming frames instead of applying them. */
  freeze(ptyId: number): void {
    this.frozenPtys.add(ptyId);
  }

  isFrozen(ptyId: number): boolean {
    return this.frozenPtys.has(ptyId);
  }

  /** Thaw a PTY: apply all buffered frames and resume normal updates. */
  thaw(ptyId: number): void {
    this.frozenPtys.delete(ptyId);
    const buf = this.frozenBuffers.get(ptyId);
    if (buf && buf.length > 0) {
      this.frozenBuffers.delete(ptyId);
      let t = this.terminals.get(ptyId);
      if (!t && this.mod) {
        t = this.createTerminal();
        this.terminals.set(ptyId, t);
      }
      if (t) {
        for (const frame of buf) {
          t.feed_compressed(frame);
        }
        for (const l of this.dirtyListeners) l(ptyId);
      }
    } else {
      this.frozenBuffers.delete(ptyId);
    }
  }

  setDesiredSubscriptions(ptyIds: Set<number>): void {
    this.desired = new Set(ptyIds);
    this.syncSubscriptions();
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
        // Move the old terminal to stale so the component can keep
        // rendering it until the first fresh frame arrives and creates
        // a new terminal — avoids a black flash.
        const old = this.terminals.get(id);
        if (old) {
          this.terminals.delete(id);
          this.staleTerminals.set(id, old);
        }
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

  private startRafProbe(): void {
    if (this.rafHandle || typeof requestAnimationFrame === "undefined") return;
    const measure = (ts: number) => {
      if (this.disposed) return;
      if (this.rafPrev > 0) {
        const dt = ts - this.rafPrev;
        if (dt > 0) {
          this.rafSamples.push(dt);
          if (this.rafSamples.length >= 20) {
            this.rafSamples.sort((a, b) => a - b);
            const median = this.rafSamples[this.rafSamples.length >> 1];
            const fps = Math.round(1_000 / median);
            this.rafSamples = [];
            if (fps > 0 && fps !== this.displayFps) {
              this.displayFps = fps;
              this.sendDisplayFps();
            }
          }
        }
      }
      this.rafPrev = ts;
      this.rafHandle = requestAnimationFrame(measure);
    };
    this.rafHandle = requestAnimationFrame(measure);
  }

  private stopRafProbe(): void {
    if (this.rafHandle) {
      cancelAnimationFrame(this.rafHandle);
      this.rafHandle = 0;
    }
  }

  private resync(): void {
    this.sendDisplayFps();
    this.subscribed.clear();
    this.syncSubscriptions();
  }

  /** Permanently destroy the store — free all WASM terminals and GL resources. */
  destroy(): void {
    this.disposed = true;
    this.stopRafProbe();
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
  }
}
