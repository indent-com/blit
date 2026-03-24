import type { Terminal } from "blit-browser";
import {
  C2S_ACK,
  C2S_SUBSCRIBE,
  C2S_UNSUBSCRIBE,
  S2C_UPDATE,
  DEFAULT_FONT,
} from "./types";
import type { BlitTransport, TerminalPalette } from "./types";


let wasmInitPromise: Promise<typeof import("blit-browser")> | null = null;

function initWasm(): Promise<typeof import("blit-browser")> {
  if (!wasmInitPromise) {
    wasmInitPromise = import("blit-browser").then(async (mod) => {
      await mod.default();
      return mod;
    });
  }
  return wasmInitPromise;
}

export type TerminalDirtyListener = (ptyId: number) => void;

export class TerminalStore {
  private mod: typeof import("blit-browser") | null = null;
  private terminals = new Map<number, Terminal>();
  private subscribed = new Set<number>();
  private desired = new Set<number>();
  private transport: BlitTransport;
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

  constructor(transport: BlitTransport) {
    this.transport = transport;

    this.onMessage = (data: ArrayBuffer) => {
      const bytes = new Uint8Array(data);
      if (bytes.length < 3 || bytes[0] !== S2C_UPDATE) return;
      const ptyId = bytes[1] | (bytes[2] << 8);
      let t = this.terminals.get(ptyId);
      if (!t) {
        if (!this.mod) return;
        t = this.createTerminal();
        this.terminals.set(ptyId, t);
      }
      t.feed_compressed(bytes.subarray(3));
      // ACK every frame immediately — don't gate on rendering.
      this.transport.send(new Uint8Array([C2S_ACK]));
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

    initWasm().then((mod) => {
      if (this.disposed) return;
      this.mod = mod;
      this.ready = true;
      for (const l of this.readyListeners) l();
    });
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
    return this.terminals.get(ptyId) ?? null;
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

  addDirtyListener(listener: TerminalDirtyListener): () => void {
    this.dirtyListeners.add(listener);
    return () => this.dirtyListeners.delete(listener);
  }

  private syncSubscriptions(): void {
    if (this.transport.status !== "connected") return;
    for (const id of this.desired) {
      if (!this.subscribed.has(id)) {
        this.subscribed.add(id);
        const msg = new Uint8Array(3);
        msg[0] = C2S_SUBSCRIBE;
        msg[1] = id & 0xff;
        msg[2] = (id >> 8) & 0xff;
        this.transport.send(msg);
      }
    }
    for (const id of this.subscribed) {
      if (!this.desired.has(id)) {
        this.subscribed.delete(id);
        const msg = new Uint8Array(3);
        msg[0] = C2S_UNSUBSCRIBE;
        msg[1] = id & 0xff;
        msg[2] = (id >> 8) & 0xff;
        this.transport.send(msg);
        const t = this.terminals.get(id);
        if (t) {
          t.free();
          this.terminals.delete(id);
        }
      }
    }
  }

  private resync(): void {
    this.subscribed.clear();
    this.syncSubscriptions();
  }

  dispose(): void {
    this.disposed = true;
    this.transport.removeEventListener("message", this.onMessage);
    this.transport.removeEventListener("statuschange", this.onStatus);
    for (const t of this.terminals.values()) t.free();
    this.terminals.clear();
    this.subscribed.clear();
    this.dirtyListeners.clear();
    this.readyListeners.clear();
  }
}
