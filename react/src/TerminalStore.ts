import type { Terminal } from "blit-browser";
import { S2C_UPDATE, DEFAULT_FONT } from "./types";
import type { BlitTransport, TerminalPalette } from "./types";
import {
  buildAckMessage,
  buildSubscribeMessage,
  buildUnsubscribeMessage,
} from "./protocol";

export type BlitWasmModule = typeof import("blit-browser");

export type TerminalDirtyListener = (ptyId: number) => void;

export class TerminalStore {
  private mod: BlitWasmModule | null = null;
  private terminals = new Map<number, Terminal>();
  private staleTerminals = new Map<number, Terminal>();
  private retainCount = new Map<number, number>();
  private pendingFree = new Set<number>();
  private subscribed = new Set<number>();
  private desired = new Set<number>();
  private _transport: BlitTransport;
  private dirtyListeners = new Set<TerminalDirtyListener>();
  private leadPtyId: number | null = null;
  private fontFamily = DEFAULT_FONT;
  private cellPw = 1;
  private cellPh = 1;
  private palette: TerminalPalette | null = null;
  private disposed = false;
  private onMessage: (data: ArrayBuffer) => void;
  private onStatus: (status: string) => void;
  private ready = false;
  private readyListeners = new Set<() => void>();

  constructor(transport: BlitTransport, wasm: BlitWasmModule | Promise<BlitWasmModule>) {
    this._transport = transport;

    this.onMessage = (data: ArrayBuffer) => {
      const bytes = new Uint8Array(data);
      if (bytes.length < 3 || bytes[0] !== S2C_UPDATE) return;
      const ptyId = bytes[1] | (bytes[2] << 8);
      let t = this.terminals.get(ptyId);
      if (!t) {
        if (!this.mod) return;
        t = this.createTerminal();
        this.terminals.set(ptyId, t);
        // Free the stale terminal now that the fresh one is ready.
        const stale = this.staleTerminals.get(ptyId);
        if (stale) {
          this.staleTerminals.delete(ptyId);
          stale.free();
        }
      }
      t.feed_compressed(bytes.subarray(3));
      // ACK every frame immediately — don't gate on rendering.
      this._transport.send(buildAckMessage());
      for (const l of this.dirtyListeners) l(ptyId);
    };

    this.onStatus = (status: string) => {
      if (status === "connected") {
        this.resync();
      } else if (status === "disconnected" || status === "error") {
        this.subscribed.clear();
      }
    };

    transport.addEventListener("message", this.onMessage);
    transport.addEventListener("statuschange", this.onStatus);

    const resolved = wasm instanceof Promise ? wasm : Promise.resolve(wasm);
    resolved
      .then((mod) => {
        if (this.disposed) return;
        this.mod = mod;
        this.ready = true;
        for (const l of this.readyListeners) l();
      })
      .catch((err) => {
        console.error("blit: failed to load WASM module:", err);
      });
  }

  get transport(): BlitTransport {
    return this._transport;
  }

  isReady(): boolean {
    return this.ready;
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
    if (typeof t.set_font_family === "function") t.set_font_family(this.fontFamily);
    if (this.palette) {
      t.set_default_colors(...this.palette.fg, ...this.palette.bg);
      for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...this.palette.ansi[i]);
    }
    return t;
  }

  getTerminal(ptyId: number): Terminal | null {
    return this.terminals.get(ptyId) ?? this.staleTerminals.get(ptyId) ?? null;
  }

  setLead(ptyId: number | null): void {
    this.leadPtyId = ptyId;
  }

  setFontFamily(fontFamily: string): void {
    this.fontFamily = fontFamily;
    for (const t of this.terminals.values()) {
      t.set_font_family(fontFamily);
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
    if (this._transport.status !== "connected") return;
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
        this._transport.send(buildSubscribeMessage(id));
      }
    }
    for (const id of this.subscribed) {
      if (!this.desired.has(id)) {
        this.subscribed.delete(id);
        this._transport.send(buildUnsubscribeMessage(id));
        // Don't free the terminal — BlitTerminal may still hold a ref.
        // It will be freed on PTY close or store dispose.
      }
    }
  }

  private resync(): void {
    this.subscribed.clear();
    this.syncSubscriptions();
  }

  dispose(): void {
    this.disposed = true;
    this._transport.removeEventListener("message", this.onMessage);
    this._transport.removeEventListener("statuschange", this.onStatus);
    for (const t of this.terminals.values()) t.free();
    this.terminals.clear();
    for (const t of this.staleTerminals.values()) t.free();
    this.staleTerminals.clear();
    this.subscribed.clear();
    this.dirtyListeners.clear();
    this.readyListeners.clear();
  }
}
