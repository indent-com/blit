import type { Terminal } from "@blit-sh/browser";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import type { TerminalPalette, ConnectionStatus, SessionId } from "./types";
import type { CellMetrics } from "./measure";
import type { GlRenderer } from "./gl-renderer";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import { measureCell, cssFontFamily } from "./measure";
import { keyToBytes, encoder } from "./keyboard";
import { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./protocol";

export interface TerminalViewOptions {
  container: HTMLElement;
  workspace: BlitWorkspace;
  sessionId: SessionId | null;
  fontFamily?: string;
  fontSize?: number;
  palette?: TerminalPalette;
  readOnly?: boolean;
  showCursor?: boolean;
  scrollbarColor?: string;
  scrollbarWidth?: number;
  advanceRatio?: number;
  onRender?: (renderMs: number) => void;
}

type SelPos = { row: number; col: number; tailOffset: number };

const isSafari = typeof navigator !== "undefined" && /^((?!chrome|android).)*safari/i.test(navigator.userAgent);

function effectiveDpr(): number {
  const base = window.devicePixelRatio || 1;
  if (isSafari && window.outerWidth && window.innerWidth) {
    const zoom = window.outerWidth / window.innerWidth;
    if (zoom > 0.25 && zoom < 8) return Math.round(base * zoom * 100) / 100;
  }
  return base;
}

export class TerminalView {
  private readonly container: HTMLElement;
  private readonly workspace: BlitWorkspace;
  private readonly canvas: HTMLCanvasElement;
  private textarea: HTMLTextAreaElement | null = null;

  private _sessionId: SessionId | null;
  private _fontFamily: string;
  private _fontSize: number;
  private _palette: TerminalPalette | undefined;
  private _readOnly: boolean;
  private _showCursor: boolean;
  private _scrollbarColor: string;
  private _scrollbarWidth: number;
  private _advanceRatio: number | undefined;
  private _onRender: ((renderMs: number) => void) | undefined;

  private blitConn: BlitConnection | null = null;
  private viewId: string | null = null;
  private terminal: Terminal | null = null;
  private renderer: GlRenderer | null = null;
  private displayCtx: CanvasRenderingContext2D | null = null;
  private cell: CellMetrics;
  private _rows = 24;
  private _cols = 80;
  private dpr: number;

  private contentDirty = true;
  private lastOffset = 0;
  private lastWasmBuffer: ArrayBuffer | null = null;
  private rafId = 0;
  private renderScheduled = false;

  private scrollOffset = 0;
  private scrollFade = 0;
  private scrollFadeTimer: ReturnType<typeof setTimeout> | null = null;
  private scrollbarGeo: {
    barX: number; barY: number; barW: number; barH: number;
    canvasH: number; totalLines: number; viewportRows: number;
  } | null = null;
  private scrollDragging = false;
  private scrollDragOffset = 0;

  private cursorBlinkOn = true;
  private cursorBlinkTimer: ReturnType<typeof setInterval> | null = null;

  private selStart: SelPos | null = null;
  private selEnd: SelPos | null = null;
  private selecting = false;
  private hoveredUrl: { row: number; startCol: number; endCol: number; url: string } | null = null;

  private predicted = "";
  private predictedFromRow = 0;
  private predictedFromCol = 0;

  private wasmReady = false;
  private disposed = false;

  private cleanups: (() => void)[] = [];
  private sessionCleanups: (() => void)[] = [];

  get rows(): number { return this._rows; }
  get cols(): number { return this._cols; }
  get currentTerminal(): Terminal | null { return this.terminal; }
  get status(): ConnectionStatus {
    if (!this.blitConn) return "disconnected";
    const snap = this.blitConn.getSnapshot();
    return snap.status;
  }

  set sessionId(id: SessionId | null) {
    if (id === this._sessionId) return;
    this.teardownSession();
    this._sessionId = id;
    this.setupSession();
  }

  set palette(p: TerminalPalette | undefined) {
    this._palette = p;
    this.applyPaletteToTerminal(this.terminal);
  }

  set showCursor(v: boolean) {
    if (v === this._showCursor) return;
    this._showCursor = v;
    this.contentDirty = true;
    this.scheduleRender();
  }

  set readOnly(v: boolean) {
    if (v === this._readOnly) return;
    this._readOnly = v;
    if (v) {
      this.removeTextarea();
      this.teardownCursorBlink();
    } else {
      this.createTextarea();
      this.setupCursorBlink();
    }
    this.teardownSession();
    this.setupSession();
  }

  set fontFamily(f: string) {
    if (f === this._fontFamily) return;
    this._fontFamily = f;
    this.applyMetrics(true);
  }

  set fontSize(s: number) {
    if (s === this._fontSize) return;
    this._fontSize = s;
    this.applyMetrics(true);
  }

  set advanceRatio(r: number | undefined) {
    if (r === this._advanceRatio) return;
    this._advanceRatio = r;
    this.applyMetrics(true);
  }

  set scrollbarColor(c: string) { this._scrollbarColor = c; this.scheduleRender(); }
  set scrollbarWidth(w: number) { this._scrollbarWidth = w; this.scheduleRender(); }
  set onRender(fn: ((ms: number) => void) | undefined) { this._onRender = fn; }

  constructor(options: TerminalViewOptions) {
    this.container = options.container;
    this.workspace = options.workspace;
    this._sessionId = options.sessionId;
    this._fontFamily = options.fontFamily ?? DEFAULT_FONT;
    this._fontSize = options.fontSize ?? DEFAULT_FONT_SIZE;
    this._palette = options.palette;
    this._readOnly = options.readOnly ?? false;
    this._showCursor = options.showCursor ?? true;
    this._scrollbarColor = options.scrollbarColor ?? "rgba(255,255,255,0.3)";
    this._scrollbarWidth = options.scrollbarWidth ?? 4;
    this._advanceRatio = options.advanceRatio;
    this._onRender = options.onRender;

    this.dpr = effectiveDpr();
    this.cell = measureCell(this._fontFamily, this._fontSize, this.dpr, this._advanceRatio);

    this.container.style.position = "relative";
    this.container.style.overflow = "hidden";

    this.canvas = document.createElement("canvas");
    if (this._readOnly) {
      Object.assign(this.canvas.style, {
        display: "block",
        width: "100%",
        height: "100%",
        objectFit: "contain",
        objectPosition: "center",
      });
    } else {
      Object.assign(this.canvas.style, {
        display: "block",
        position: "absolute",
        top: "0",
        left: "0",
        cursor: "text",
      });
    }
    this.container.appendChild(this.canvas);

    if (!this._readOnly) {
      this.createTextarea();
    }

    this.resolveConnection();
    this.setupDprDetection();
    this.setupCursorBlink();
    this.setupGlRenderer();
    this.setupSession();
    this.setupResizeObserver();
    this.setupRenderLoop();
    this.setupKeyboard();
    this.setupWheelScroll();
    this.setupMouse();
    this.setupFontLoading();
    this.setupWasmReady();
  }

  focus(): void {
    this.textarea?.focus();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.teardownSession();
    for (const fn of this.cleanups) fn();
    this.cleanups.length = 0;
    cancelAnimationFrame(this.rafId);
    this.teardownCursorBlink();
    if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
    this.canvas.remove();
    this.textarea?.remove();
  }

  private createTextarea(): void {
    if (this.textarea) return;
    const ta = document.createElement("textarea");
    ta.setAttribute("aria-label", "Terminal input");
    ta.setAttribute("autocapitalize", "off");
    ta.setAttribute("autocomplete", "off");
    ta.setAttribute("autocorrect", "off");
    ta.spellcheck = false;
    ta.tabIndex = 0;
    Object.assign(ta.style, {
      position: "absolute",
      opacity: "0",
      width: "1px",
      height: "1px",
      top: "0",
      left: "0",
      padding: "0",
      border: "none",
      outline: "none",
      resize: "none",
      overflow: "hidden",
    });
    this.container.appendChild(ta);
    this.textarea = ta;
  }

  private removeTextarea(): void {
    if (!this.textarea) return;
    this.textarea.remove();
    this.textarea = null;
  }

  private resolveConnection(): void {
    if (this._sessionId === null) {
      this.blitConn = null;
      return;
    }
    const snap = this.workspace.getSnapshot();
    const session = snap.sessions.find((s) => s.id === this._sessionId);
    if (!session) {
      this.blitConn = null;
      return;
    }
    this.blitConn = this.workspace.getConnection(session.connectionId);
  }

  private scheduleRender = (): void => {
    if (this.renderScheduled || this.disposed) return;
    this.renderScheduled = true;
    this.rafId = requestAnimationFrame(() => {
      this.renderScheduled = false;
      this.doRender();
    });
  };

  private reconcilePrediction(): void {
    const t = this.terminal;
    if (!t || !this.predicted) return;
    const cr = t.cursor_row;
    const cc = t.cursor_col;
    if (cr !== this.predictedFromRow) {
      this.predicted = "";
      return;
    }
    const advance = cc - this.predictedFromCol;
    if (advance > 0 && advance <= this.predicted.length) {
      this.predicted = this.predicted.slice(advance);
      this.predictedFromCol = cc;
    } else if (advance < 0 || advance > this.predicted.length) {
      this.predicted = "";
    }
  }

  private applyPaletteToTerminal(t: Terminal | null): void {
    const p = this._palette;
    if (!t || !p) return;
    t.set_default_colors(...p.fg, ...p.bg);
    for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...p.ansi[i]);
    this.contentDirty = true;
    this.scheduleRender();
  }

  private applyMetricsToTerminal(t: Terminal): void {
    const cell = this.cell;
    t.set_cell_size(cell.pw, cell.ph);
    t.set_font_family(this._fontFamily);
    t.set_font_size(this._fontSize * this.dpr);
    t.invalidate_render_cache();
  }

  private applyMetrics(forceInvalidate = false): void {
    if (!this.blitConn) return;
    const rasterFontSize = this._fontSize * this.dpr;
    const cell = measureCell(this._fontFamily, this._fontSize, this.dpr, this._advanceRatio);
    const changed = cell.pw !== this.cell.pw || cell.ph !== this.cell.ph;
    const shouldInvalidate = forceInvalidate || changed;
    this.cell = cell;
    if (!this._readOnly) {
      const t = this.terminal;
      if (t) {
        t.set_cell_size(cell.pw, cell.ph);
        t.set_font_family(this._fontFamily);
        t.set_font_size(rasterFontSize);
        if (shouldInvalidate) t.invalidate_render_cache();
      }
      this.blitConn.setCellSize(cell.pw, cell.ph);
      this.blitConn.setFontFamily(this._fontFamily);
      this.blitConn.setFontSize(rasterFontSize);
    }
    if (shouldInvalidate) {
      this.contentDirty = true;
      this.scheduleRender();
    }
    if (changed) {
      this.handleResize();
    }
  }

  private setupWasmReady(): void {
    if (!this.blitConn) return;
    const conn = this.blitConn;
    if (conn.isReady()) this.wasmReady = true;
    const unsub = conn.onReady(() => {
      this.wasmReady = true;
      this.setupSession();
    });
    this.cleanups.push(unsub);
  }

  private setupDprDetection(): void {
    const check = () => {
      const next = effectiveDpr();
      if (next !== this.dpr) {
        this.dpr = next;
        this.applyMetrics(true);
      }
    };
    let mq: MediaQueryList | null = null;
    if (typeof window.matchMedia === "function") {
      mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
      mq.addEventListener("change", check);
    }
    window.addEventListener("resize", check);
    this.cleanups.push(() => {
      mq?.removeEventListener("change", check);
      window.removeEventListener("resize", check);
    });
  }

  private setupCursorBlink(): void {
    if (this._readOnly) return;
    this.cursorBlinkOn = true;
    this.cursorBlinkTimer = setInterval(() => {
      this.cursorBlinkOn = !this.cursorBlinkOn;
      this.scheduleRender();
    }, 530);
  }

  private teardownCursorBlink(): void {
    if (this.cursorBlinkTimer) {
      clearInterval(this.cursorBlinkTimer);
      this.cursorBlinkTimer = null;
    }
  }

  private setupGlRenderer(): void {
    if (!this.blitConn) return;
    const shared = this.blitConn.getSharedRenderer();
    if (shared) this.renderer = shared.renderer;
  }

  private setupFontLoading(): void {
    const onFontsLoaded = () => this.applyMetrics(true);
    document.fonts?.addEventListener("loadingdone", onFontsLoaded);
    if (document.fonts?.status === "loaded") this.applyMetrics(true);
    this.cleanups.push(() => document.fonts?.removeEventListener("loadingdone", onFontsLoaded));
  }

  private setupSession(): void {
    this.resolveConnection();
    const conn = this.blitConn;
    const sid = this._sessionId;

    if (!conn || sid === null) {
      this.terminal = null;
      return;
    }

    conn.retain(sid);
    this.sessionCleanups.push(() => conn.release(sid));

    if (!this._readOnly) {
      this.viewId = conn.allocViewId();
      const vid = this.viewId;
      this.sessionCleanups.push(() => conn.removeView(sid, vid));
    }

    const t = conn.getTerminal(sid);
    if (t) {
      this.terminal = t;
      this.applyPaletteToTerminal(t);
      if (!this._readOnly) {
        this.applyMetricsToTerminal(t);
      }
      this.contentDirty = true;
      this.scheduleRender();
    }

    const syncReadOnlySize = (t2: Terminal) => {
      const tr = t2.rows;
      const tc = t2.cols;
      if (tr !== this._rows || tc !== this._cols) {
        this._rows = tr;
        this._cols = tc;
      }
      this.scheduleRender();
    };

    const unsub = conn.addDirtyListener(sid, () => {
      const t2 = conn.getTerminal(sid);
      if (!t2) return;
      if (this.terminal !== t2) {
        this.terminal = t2;
        this.applyPaletteToTerminal(t2);
        this.applyMetricsToTerminal(t2);
      }
      this.contentDirty = true;
      this.scheduleRender();
      this.reconcilePrediction();
      if (this._readOnly) syncReadOnlySize(t2);
    });
    this.sessionCleanups.push(unsub);

    if (t) {
      if (this._readOnly) syncReadOnlySize(t);
    }
  }

  private teardownSession(): void {
    for (const fn of this.sessionCleanups) fn();
    this.sessionCleanups.length = 0;
    this.terminal = null;
  }

  private handleResize = (): void => {
    if (this._readOnly || !this.blitConn) return;
    const cell = this.cell;
    const w = this.container.clientWidth;
    const h = this.container.clientHeight;
    const cols = Math.max(1, Math.floor(w / cell.w));
    const rows = Math.max(1, Math.floor(h / cell.h));

    const sizeChanged = cols !== this._cols || rows !== this._rows;
    if (sizeChanged) {
      this._rows = rows;
      this._cols = cols;
      if (this._sessionId !== null && this.viewId) {
        this.blitConn.setViewSize(this._sessionId, this.viewId, rows, cols);
      }
    }
    this.contentDirty = true;
    this.scheduleRender();
  };

  private setupResizeObserver(): void {
    if (this._readOnly) return;

    const observer = new ResizeObserver(this.handleResize);
    observer.observe(this.container);
    window.addEventListener("resize", this.handleResize);
    this.handleResize();

    this.cleanups.push(() => {
      observer.disconnect();
      window.removeEventListener("resize", this.handleResize);
    });

    const checkStatus = () => {
      const snap = this.blitConn?.getSnapshot();
      if (
        snap?.status === "connected" &&
        this._sessionId !== null &&
        !this._readOnly &&
        this.blitConn &&
        this.viewId
      ) {
        const rows = this._rows;
        const cols = this._cols;
        if (rows > 0 && cols > 0) {
          this.blitConn.setViewSize(this._sessionId, this.viewId, rows, cols);
        }
      }
    };

    if (this.blitConn) {
      const unsub = this.blitConn.subscribe(checkStatus);
      this.cleanups.push(unsub);
    }
  }

  private setupRenderLoop(): void {
    this.scheduleRender();
    this.cleanups.push(() => {
      cancelAnimationFrame(this.rafId);
      this.renderScheduled = false;
    });
  }

  private doRender(): void {
    const conn = this.blitConn;
    if (!conn) return;

    const t0 = performance.now();
    if (!this.renderer?.supported) {
      const shared = conn.getSharedRenderer();
      if (shared) this.renderer = shared.renderer;
      if (!this.renderer?.supported) {
        if (!this._readOnly) conn.noteFrameRendered();
        return;
      }
    }
    if (!this.terminal) {
      if (!this._readOnly) conn.noteFrameRendered();
      return;
    }

    const t = this.terminal;
    const cell = this.cell;
    const renderer = this.renderer;

    const termCols = t.cols;
    const termRows = t.rows;
    const pw = termCols * cell.pw;
    const ph = termRows * cell.ph;

    if (!this._readOnly) {
      const cssW = `${termCols * cell.w}px`;
      const cssH = `${termRows * cell.h}px`;
      if (this.canvas.style.width !== cssW) this.canvas.style.width = cssW;
      if (this.canvas.style.height !== cssH) this.canvas.style.height = cssH;
    }

    const mem = conn.wasmMemory()!;
    if (mem.buffer !== this.lastWasmBuffer) {
      this.lastWasmBuffer = mem.buffer;
      this.contentDirty = true;
    }

    {
      const gridH = t.rows * cell.ph;
      const gridW = t.cols * cell.pw;
      const xOff = Math.max(0, Math.floor((pw - gridW) / 2));
      const yOff = Math.max(0, Math.floor((ph - gridH) / 2));
      const combined = xOff * 65536 + yOff;
      if (combined !== this.lastOffset) {
        this.lastOffset = combined;
        t.set_render_offset(xOff, yOff);
        this.contentDirty = true;
      }
    }

    if (this.contentDirty) {
      this.contentDirty = false;
      t.prepare_render_ops();
    }

    const bgVerts = new Float32Array(mem.buffer, t.bg_verts_ptr(), t.bg_verts_len());
    const glyphVerts = new Float32Array(mem.buffer, t.glyph_verts_ptr(), t.glyph_verts_len());
    renderer.resize(pw, ph);
    renderer.render(
      bgVerts, glyphVerts,
      t.glyph_atlas_canvas(), t.glyph_atlas_version(),
      t.cursor_visible(), t.cursor_col, t.cursor_row, t.cursor_style(),
      this.cursorBlinkOn, cell,
      this._palette?.bg ?? [0, 0, 0],
      this._showCursor,
    );

    const shared = conn.getSharedRenderer();
    const displayCanvas = this.canvas;
    if (shared && displayCanvas) {
      if (displayCanvas.width !== pw) {
        displayCanvas.width = pw;
        this.displayCtx = null;
      }
      if (displayCanvas.height !== ph) {
        displayCanvas.height = ph;
        this.displayCtx = null;
      }
      if (!this.displayCtx) {
        this.displayCtx = displayCanvas.getContext("2d");
        this.displayCtx?.resetTransform();
      }
      const ctx = this.displayCtx;
      if (ctx) {
        ctx.drawImage(shared.canvas, 0, 0, pw, ph, 0, 0, pw, ph);
        this.renderSelectionOverlay(ctx, cell);
        this.renderUrlUnderline(ctx, cell);
        this.renderOverflowText(ctx, t, cell);
        this.renderPredictedEcho(ctx, t, cell);
        this.renderScrollbar(ctx, t, cell);
      }
    }

    if (!this._readOnly) {
      conn.noteFrameRendered();
    }
    this._onRender?.(performance.now() - t0);
  }

  private renderSelectionOverlay(ctx: CanvasRenderingContext2D, cell: CellMetrics): void {
    const ss = this.selStart;
    const se = this.selEnd;
    if (!ss || !se) return;
    const curScroll = this.scrollOffset;
    const rows = this._rows;
    const toViewRow = (p: SelPos) => rows - 1 - p.tailOffset + curScroll;
    let sr = toViewRow(ss), sc = ss.col;
    let er = toViewRow(se), ec = se.col;
    if (sr > er || (sr === er && sc > ec)) {
      [sr, sc, er, ec] = [er, ec, sr, sc];
    }
    const r0 = Math.max(0, sr);
    const r1 = Math.min(rows - 1, er);
    ctx.fillStyle = "rgba(100,150,255,0.3)";
    for (let r = r0; r <= r1; r++) {
      const c0 = r === sr ? sc : 0;
      const c1 = r === er ? ec : this._cols - 1;
      ctx.fillRect(c0 * cell.pw, r * cell.ph, (c1 - c0 + 1) * cell.pw, cell.ph);
    }
  }

  private renderUrlUnderline(ctx: CanvasRenderingContext2D, cell: CellMetrics): void {
    const hurl = this.hoveredUrl;
    if (!hurl) return;
    const [fgR, fgG, fgB] = this._palette?.fg ?? [204, 204, 204];
    ctx.strokeStyle = `rgba(${fgR},${fgG},${fgB},0.6)`;
    ctx.lineWidth = Math.max(1, Math.round(cell.ph * 0.06));
    const y = hurl.row * cell.ph + cell.ph - ctx.lineWidth;
    ctx.beginPath();
    ctx.moveTo(hurl.startCol * cell.pw, y);
    ctx.lineTo((hurl.endCol + 1) * cell.pw, y);
    ctx.stroke();
  }

  private renderOverflowText(ctx: CanvasRenderingContext2D, t: Terminal, cell: CellMetrics): void {
    const overflowCount = t.overflow_text_count();
    if (overflowCount === 0) return;
    const cw = cell.pw;
    const ch = cell.ph;
    const scale = 0.85;
    const scaledH = ch * scale;
    const fSize = Math.max(1, Math.round(scaledH));
    ctx.font = `${fSize}px ${cssFontFamily(this._fontFamily)}`;
    ctx.textBaseline = "bottom";
    const [fgR, fgG, fgB] = this._palette?.fg ?? [204, 204, 204];
    ctx.fillStyle = `#${fgR.toString(16).padStart(2, "0")}${fgG.toString(16).padStart(2, "0")}${fgB.toString(16).padStart(2, "0")}`;
    for (let i = 0; i < overflowCount; i++) {
      const op = t.overflow_text_op(i);
      if (!op) continue;
      const [row, col, colSpan, text] = op as [number, number, number, string];
      const x = col * cw;
      const y = row * ch;
      const w = colSpan * cw;
      const padX = (w - w * scale) / 2;
      const padY = (ch - scaledH) / 2;
      ctx.save();
      ctx.beginPath();
      ctx.rect(x, y, w, ch);
      ctx.clip();
      ctx.fillText(text, x + padX, y + padY + scaledH);
      ctx.restore();
    }
  }

  private renderPredictedEcho(ctx: CanvasRenderingContext2D, t: Terminal, cell: CellMetrics): void {
    if (this._readOnly || !this.predicted) return;
    if (!t.echo()) return;
    const cw = cell.pw;
    const ch = cell.ph;
    const [fR, fG, fB] = this._palette?.fg ?? [204, 204, 204];
    ctx.fillStyle = `rgba(${fR},${fG},${fB},0.5)`;
    const fSize = Math.max(1, Math.round(ch * 0.85));
    ctx.font = `${fSize}px ${cssFontFamily(this._fontFamily)}`;
    ctx.textBaseline = "bottom";
    const pred = this.predicted;
    const cc = t.cursor_col;
    const cr = t.cursor_row;
    for (let i = 0; i < pred.length && cc + i < this._cols; i++) {
      ctx.fillText(pred[i], (cc + i) * cw, cr * ch + ch);
    }
  }

  private renderScrollbar(ctx: CanvasRenderingContext2D, t: Terminal, cell: CellMetrics): void {
    const totalLines = t.scrollback_lines() + this._rows;
    const viewportRows = this._rows;
    if (totalLines <= viewportRows) {
      this.scrollbarGeo = null;
      return;
    }
    const ch = cell.ph;
    const canvasH = viewportRows * ch;
    const barW = this._scrollbarWidth;
    const barH = Math.max(barW, (viewportRows / totalLines) * canvasH);
    const maxScroll = totalLines - viewportRows;
    const scrollFraction = Math.min(this.scrollOffset, maxScroll) / maxScroll;
    const barY = (1 - scrollFraction) * (canvasH - barH);
    const barX = this._cols * cell.pw - barW - 2;
    this.scrollbarGeo = { barX, barY, barW, barH, canvasH, totalLines, viewportRows };
    const show = this.scrollFade > 0 || this.scrollDragging || this.scrollOffset > 0;
    if (show) {
      ctx.fillStyle = this._scrollbarColor;
      ctx.beginPath();
      ctx.roundRect(barX, barY, barW, barH, barW / 2);
      ctx.fill();
    }
  }

  private getStatus(): ConnectionStatus {
    if (!this.blitConn) return "disconnected";
    return this.blitConn.getSnapshot().status;
  }

  private setupKeyboard(): void {
    if (this._readOnly) return;
    const input = this.textarea;
    if (!input) return;

    const handleKeyDown = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      if (this._sessionId === null || this.getStatus() !== "connected") return;
      if (e.isComposing) return;
      if (e.key === "Dead") return;
      const sid = this._sessionId;

      if (e.shiftKey && (e.key === "PageUp" || e.key === "PageDown")) {
        const t2 = this.terminal;
        const maxScroll = t2 ? t2.scrollback_lines() : 0;
        if (maxScroll > 0 || this.scrollOffset > 0) {
          e.preventDefault();
          const delta = e.key === "PageUp" ? this._rows : -this._rows;
          this.scrollOffset = Math.max(0, Math.min(maxScroll, this.scrollOffset + delta));
          this.workspace.scrollSession(sid, this.scrollOffset);
          this.startScrollFade();
          this.scheduleRender();
        }
        return;
      }
      if (e.shiftKey && (e.key === "Home" || e.key === "End")) {
        const t2 = this.terminal;
        const maxScroll = t2 ? t2.scrollback_lines() : 0;
        if (maxScroll > 0 || this.scrollOffset > 0) {
          e.preventDefault();
          this.scrollOffset = e.key === "Home" ? maxScroll : 0;
          this.workspace.scrollSession(sid, this.scrollOffset);
          this.startScrollFade();
          this.scheduleRender();
        }
        return;
      }

      const t = this.terminal;
      const appCursor = t ? t.app_cursor() : false;
      const bytes = keyToBytes(e, appCursor);
      if (bytes) {
        e.preventDefault();
        if (this.scrollOffset > 0) {
          this.scrollOffset = 0;
          this.workspace.scrollSession(sid, 0);
        }
        if (t && t.echo() && e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
          if (!this.predicted) {
            this.predictedFromRow = t.cursor_row;
            this.predictedFromCol = t.cursor_col;
          }
          this.predicted += e.key;
          this.scheduleRender();
        } else {
          this.predicted = "";
        }
        this.workspace.sendInput(sid, bytes);
      }
    };

    const handleCompositionEnd = (e: CompositionEvent) => {
      if (e.data && this._sessionId !== null && this.getStatus() === "connected") {
        this.workspace.sendInput(this._sessionId, encoder.encode(e.data));
      }
      input.value = "";
    };

    const handleInput = (e: Event) => {
      const inputEvent = e as InputEvent;
      if (inputEvent.isComposing) {
        if (
          inputEvent.inputType === "deleteContentBackward" &&
          !input.value &&
          this._sessionId !== null &&
          this.getStatus() === "connected"
        ) {
          this.workspace.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
        return;
      }
      if (inputEvent.inputType === "deleteContentBackward" && !input.value) {
        if (this._sessionId !== null && this.getStatus() === "connected") {
          this.workspace.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
      } else if (input.value && this._sessionId !== null && this.getStatus() === "connected") {
        this.workspace.sendInput(this._sessionId, encoder.encode(input.value.replace(/\n/g, "\r")));
      }
      input.value = "";
    };

    input.addEventListener("keydown", handleKeyDown);
    input.addEventListener("compositionend", handleCompositionEnd);
    input.addEventListener("input", handleInput);

    this.cleanups.push(() => {
      input.removeEventListener("keydown", handleKeyDown);
      input.removeEventListener("compositionend", handleCompositionEnd);
      input.removeEventListener("input", handleInput);
    });
  }

  private startScrollFade(): void {
    this.scrollFade = 1;
    if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
    this.scrollFadeTimer = setTimeout(() => {
      this.scrollFade = 0;
      this.scheduleRender();
    }, 1000);
  }

  private setupWheelScroll(): void {
    if (this._readOnly) return;
    const container = this.container;

    const handleWheel = (e: WheelEvent) => {
      const t = this.terminal;
      if (t && t.mouse_mode() > 0 && !e.shiftKey) return;
      if (this._sessionId !== null && this.getStatus() === "connected") {
        const maxScroll = t ? t.scrollback_lines() : 0;
        if (maxScroll === 0 && this.scrollOffset === 0) return;
        e.preventDefault();
        const delta = Math.abs(e.deltaY) > Math.abs(e.deltaX) ? e.deltaY : e.deltaX;
        const lines = Math.round(-delta / 20) || (delta > 0 ? -3 : 3);
        this.scrollOffset = Math.max(0, Math.min(maxScroll, this.scrollOffset + lines));
        this.workspace.scrollSession(this._sessionId, this.scrollOffset);
        if (this.scrollOffset > 0) {
          this.startScrollFade();
        }
        this.scheduleRender();
      }
    };

    container.addEventListener("wheel", handleWheel, { passive: false });
    this.cleanups.push(() => container.removeEventListener("wheel", handleWheel));
  }

  private setupMouse(): void {
    if (this._readOnly || !this.blitConn) return;
    const canvas = this.canvas;
    const conn = this.blitConn;

    const mouseToCell = (e: MouseEvent): { row: number; col: number } => {
      const rect = canvas.getBoundingClientRect();
      const cell = this.cell;
      return {
        row: Math.min(Math.max(Math.floor((e.clientY - rect.top) / cell.h), 0), this._rows - 1),
        col: Math.min(Math.max(Math.floor((e.clientX - rect.left) / cell.w), 0), this._cols - 1),
      };
    };

    const SCROLLBAR_HIT_PX = 20;

    const canvasYFromEvent = (e: MouseEvent): number => {
      const rect = canvas.getBoundingClientRect();
      const dpr = this.cell.pw / this.cell.w;
      return (e.clientY - rect.top) * dpr;
    };

    const isNearScrollbar = (e: MouseEvent): boolean => {
      const rect = canvas.getBoundingClientRect();
      return e.clientX >= rect.right - SCROLLBAR_HIT_PX;
    };

    const scrollToCanvasY = (y: number) => {
      const geo = this.scrollbarGeo;
      if (!geo || this._sessionId === null || this.getStatus() !== "connected") return;
      const fraction = 1 - y / (geo.canvasH - geo.barH);
      const maxScroll = geo.totalLines - geo.viewportRows;
      const offset = Math.round(Math.max(0, Math.min(maxScroll, fraction * maxScroll)));
      this.scrollOffset = offset;
      this.workspace.scrollSession(this._sessionId, offset);
      this.scrollFade = 1;
      this.scheduleRender();
    };

    const sendMouseEvent = (type: "down" | "up" | "move", e: MouseEvent, button: number): boolean => {
      if (this._sessionId === null || this.getStatus() !== "connected") return false;
      const t = this.terminal;
      if (t && t.mouse_mode() === 0) return false;
      const pos = mouseToCell(e);
      const typeCode = type === "down" ? MOUSE_DOWN : type === "up" ? MOUSE_UP : MOUSE_MOVE;
      this.workspace.sendMouse(this._sessionId, typeCode, button, pos.col, pos.row);
      return true;
    };

    let selecting = false;
    let selGranularity: 1 | 2 | 3 = 1;
    let selAnchorStart: SelPos | null = null;
    let selAnchorEnd: SelPos | null = null;

    const cellToSel = (cell: { row: number; col: number }): SelPos => ({
      row: cell.row,
      col: cell.col,
      tailOffset: this.scrollOffset + (this._rows - 1 - cell.row),
    });

    let autoScrollTimer: ReturnType<typeof setInterval> | null = null;
    let autoScrollDir: -1 | 0 | 1 = 0;
    const AUTO_SCROLL_INTERVAL_MS = 50;
    const AUTO_SCROLL_LINES = 3;

    const stopAutoScroll = () => {
      if (autoScrollTimer !== null) {
        clearInterval(autoScrollTimer);
        autoScrollTimer = null;
      }
      autoScrollDir = 0;
    };

    const WORD_CHARS = /[A-Za-z0-9_\-./~:@]/;

    const getRowText = (row: number): string => {
      const t = this.terminal;
      if (!t) return "";
      return t.get_text(row, 0, row, this._cols - 1);
    };

    const wordBoundsAt = (row: number, col: number): { start: number; end: number } => {
      const text = getRowText(row);
      if (col >= text.length || !WORD_CHARS.test(text[col])) {
        return { start: col, end: col };
      }
      let start = col;
      while (start > 0 && WORD_CHARS.test(text[start - 1])) start--;
      let end = col;
      while (end < text.length - 1 && WORD_CHARS.test(text[end + 1])) end++;
      return { start, end };
    };

    const isWrapped = (row: number): boolean => {
      const t = this.terminal;
      return t ? t.is_wrapped(row) : false;
    };

    const logicalLineRange = (row: number): { startRow: number; endRow: number } => {
      const maxRow = this._rows - 1;
      let startRow = row;
      while (startRow > 0 && isWrapped(startRow - 1)) startRow--;
      let endRow = row;
      while (endRow < maxRow && isWrapped(endRow)) endRow++;
      return { startRow, endRow };
    };

    const applyGranularity = (cell: { row: number; col: number }): { start: { row: number; col: number }; end: { row: number; col: number } } => {
      if (selGranularity === 3) {
        const { startRow, endRow } = logicalLineRange(cell.row);
        return { start: { row: startRow, col: 0 }, end: { row: endRow, col: this._cols - 1 } };
      }
      if (selGranularity === 2) {
        const wb = wordBoundsAt(cell.row, cell.col);
        return { start: { row: cell.row, col: wb.start }, end: { row: cell.row, col: wb.end } };
      }
      return { start: cell, end: cell };
    };

    const applyGranularitySel = (pos: SelPos): { start: SelPos; end: SelPos } => {
      const curScroll = this.scrollOffset;
      const viewRow = this._rows - 1 - pos.tailOffset + curScroll;
      const cell = { row: viewRow, col: pos.col };
      const { start, end } = applyGranularity(cell);
      return {
        start: { ...start, tailOffset: curScroll + (this._rows - 1 - start.row) },
        end: { ...end, tailOffset: curScroll + (this._rows - 1 - end.row) },
      };
    };

    const selPosBefore = (a: SelPos, b: SelPos): boolean =>
      a.tailOffset > b.tailOffset || (a.tailOffset === b.tailOffset && a.col < b.col);

    const clearSelection = () => {
      this.selStart = this.selEnd = null;
      this.scheduleRender();
    };

    const drawSelection = () => this.scheduleRender();

    const copySelection = () => {
      if (!this.selStart || !this.selEnd) return;
      const t = this.terminal;
      if (!t) return;
      let start = this.selStart;
      let end = this.selEnd;
      if (selPosBefore(end, start)) [start, end] = [end, start];
      const curScroll = this.scrollOffset;
      const rows = this._rows;
      const startViewRow = rows - 1 - start.tailOffset + curScroll;
      const endViewRow = rows - 1 - end.tailOffset + curScroll;
      const inViewport = startViewRow >= 0 && startViewRow < rows && endViewRow >= 0 && endViewRow < rows;
      if (inViewport) {
        const text = t.get_text(startViewRow, start.col, endViewRow, end.col);
        if (text) navigator.clipboard.writeText(text);
      } else if (conn.supportsCopyRange() && this._sessionId !== null) {
        conn
          .copyRange(this._sessionId, start.tailOffset, start.col, end.tailOffset, end.col)
          .then((text) => { if (text) navigator.clipboard.writeText(text); })
          .catch(() => {});
      }
    };

    const startAutoScroll = (dir: -1 | 1) => {
      if (autoScrollDir === dir && autoScrollTimer !== null) return;
      stopAutoScroll();
      autoScrollDir = dir;
      autoScrollTimer = setInterval(() => {
        if (!selecting || this._sessionId === null || this.getStatus() !== "connected") {
          stopAutoScroll();
          return;
        }
        const t = this.terminal;
        if (!t) return;
        const maxScroll = t.scrollback_lines();
        const prev = this.scrollOffset;
        const next = Math.max(0, Math.min(maxScroll, prev + dir * AUTO_SCROLL_LINES));
        if (next === prev) return;
        this.scrollOffset = next;
        this.workspace.scrollSession(this._sessionId!, next);
        this.startScrollFade();
        const edgeRow = dir === 1 ? 0 : this._rows - 1;
        const edgeCol = dir === 1 ? 0 : this._cols - 1;
        const edgeSel = cellToSel({ row: edgeRow, col: edgeCol });
        if (selGranularity >= 2 && selAnchorStart && selAnchorEnd) {
          const { start: dragStart, end: dragEnd } = applyGranularitySel(edgeSel);
          const dragBefore = selPosBefore(dragStart, selAnchorStart);
          if (dragBefore) {
            this.selStart = dragStart;
            this.selEnd = selAnchorEnd;
          } else {
            this.selStart = selAnchorStart;
            this.selEnd = dragEnd;
          }
        } else {
          this.selEnd = edgeSel;
        }
        drawSelection();
      }, AUTO_SCROLL_INTERVAL_MS);
    };

    let mouseDownButton = -1;
    let lastMouseCell = { row: -1, col: -1 };

    const handleMouseDown = (e: MouseEvent) => {
      if (e.button === 0 && this.scrollbarGeo && isNearScrollbar(e)) {
        e.preventDefault();
        const geo = this.scrollbarGeo;
        const y = canvasYFromEvent(e);
        this.scrollDragging = true;
        canvas.style.cursor = "grabbing";
        if (y >= geo.barY && y <= geo.barY + geo.barH) {
          this.scrollDragOffset = y - geo.barY;
        } else {
          this.scrollDragOffset = geo.barH / 2;
          scrollToCanvasY(y - geo.barH / 2);
        }
        return;
      }
      if (!e.shiftKey && sendMouseEvent("down", e, e.button)) {
        mouseDownButton = e.button;
        e.preventDefault();
        return;
      }
      if (e.button === 0) {
        clearSelection();
        selecting = true;
        this.selecting = true;
        const cell = mouseToCell(e);
        const sel = cellToSel(cell);
        const detail = Math.min(e.detail, 3) as 1 | 2 | 3;
        selGranularity = detail;
        if (detail >= 2) {
          const { start, end } = applyGranularitySel(sel);
          this.selStart = start;
          this.selEnd = end;
          selAnchorStart = start;
          selAnchorEnd = end;
          drawSelection();
        } else {
          this.selStart = sel;
          this.selEnd = sel;
          selAnchorStart = null;
          selAnchorEnd = null;
        }
      }
    };

    const handleMouseMove = (e: MouseEvent) => {
      if (this.scrollDragging) {
        scrollToCanvasY(canvasYFromEvent(e) - this.scrollDragOffset);
        return;
      }
      const overCanvas = mouseDownButton >= 0 || canvas.contains(e.target as Node);
      if (!e.shiftKey && overCanvas) {
        const t = this.terminal;
        if (t) {
          const mode = t.mouse_mode();
          if (mode >= 3) {
            const cell = mouseToCell(e);
            if (cell.row === lastMouseCell.row && cell.col === lastMouseCell.col) return;
            lastMouseCell = cell;
            if (e.buttons) {
              const button = e.buttons & 1 ? 0 : e.buttons & 2 ? 2 : e.buttons & 4 ? 1 : 0;
              sendMouseEvent("move", e, button + 32);
              return;
            } else if (mode === 4) {
              sendMouseEvent("move", e, 35);
              return;
            }
          }
        }
      }
      if (selecting) {
        const rect = canvas.getBoundingClientRect();
        if (e.clientY < rect.top) {
          startAutoScroll(1);
          return;
        } else if (e.clientY > rect.bottom) {
          startAutoScroll(-1);
          return;
        } else {
          stopAutoScroll();
        }
        const cell = mouseToCell(e);
        const sel = cellToSel(cell);
        if (selGranularity >= 2 && selAnchorStart && selAnchorEnd) {
          const { start: dragStart, end: dragEnd } = applyGranularitySel(sel);
          const dragBefore = selPosBefore(dragStart, selAnchorStart);
          if (dragBefore) {
            this.selStart = dragStart;
            this.selEnd = selAnchorEnd;
          } else {
            this.selStart = selAnchorStart;
            this.selEnd = dragEnd;
          }
        } else {
          this.selEnd = sel;
        }
        drawSelection();
      }
    };

    const handleMouseUp = (e: MouseEvent) => {
      if (this.scrollDragging) {
        this.scrollDragging = false;
        canvas.style.cursor = "text";
        this.scheduleRender();
        return;
      }
      if (mouseDownButton >= 0) {
        sendMouseEvent("up", e, mouseDownButton);
        mouseDownButton = -1;
        return;
      }
      if (selecting) {
        stopAutoScroll();
        selecting = false;
        this.selecting = false;
        if (selGranularity === 1) {
          this.selEnd = cellToSel(mouseToCell(e));
        }
        drawSelection();
        if (
          this.selStart && this.selEnd &&
          (this.selStart.tailOffset !== this.selEnd.tailOffset || this.selStart.col !== this.selEnd.col)
        ) {
          copySelection();
        }
        clearSelection();
      }
      if (canvas.contains(e.target as Node)) {
        this.textarea?.focus();
      }
    };

    const handleWheel = (e: WheelEvent) => {
      const t = this.terminal;
      if (t && t.mouse_mode() > 0 && !e.shiftKey) {
        e.preventDefault();
        const button = e.deltaY < 0 ? 64 : 65;
        sendMouseEvent("down", e, button);
      }
    };

    const URL_RE = /https?:\/\/[^\s<>"'`)\]},;]+/g;

    const urlAt = (row: number, col: number): { url: string; startCol: number; endCol: number } | null => {
      const text = getRowText(row);
      URL_RE.lastIndex = 0;
      let m: RegExpExecArray | null;
      while ((m = URL_RE.exec(text)) !== null) {
        const startCol = m.index;
        const raw = m[0].replace(/[.),:;]+$/, "");
        const endCol = startCol + raw.length - 1;
        if (col >= startCol && col <= endCol) return { url: raw, startCol, endCol };
      }
      return null;
    };

    const handleContextMenu = (e: MouseEvent) => {
      const t = this.terminal;
      if (t && t.mouse_mode() > 0) e.preventDefault();
    };

    const handleClick = (e: MouseEvent) => {
      if (e.altKey && e.button === 0) {
        const cell = mouseToCell(e);
        const hit = urlAt(cell.row, cell.col);
        if (hit) {
          e.preventDefault();
          window.open(hit.url, "_blank", "noopener");
          return;
        }
      }
      this.textarea?.focus();
    };

    let lastHoverUrl: string | null = null;
    const handleHoverMove = (e: MouseEvent) => {
      if (this.scrollDragging) {
        canvas.style.cursor = "grabbing";
        return;
      }
      if (this.scrollbarGeo && isNearScrollbar(e)) {
        canvas.style.cursor = "default";
        return;
      }
      if (selecting) {
        if (this.hoveredUrl) {
          this.hoveredUrl = null;
          this.scheduleRender();
          canvas.style.cursor = "text";
          lastHoverUrl = null;
        }
        return;
      }
      const cell = mouseToCell(e);
      const hit = urlAt(cell.row, cell.col);
      const url = hit?.url ?? null;
      if (url !== lastHoverUrl) {
        lastHoverUrl = url;
        canvas.style.cursor = hit ? "pointer" : "text";
        this.hoveredUrl = hit ? { row: cell.row, startCol: hit.startCol, endCol: hit.endCol, url: hit.url } : null;
        this.scheduleRender();
      }
    };

    const handleBlur = () => {
      if (mouseDownButton >= 0) {
        if (this._sessionId !== null && this.getStatus() === "connected") {
          this.workspace.sendMouse(this._sessionId, MOUSE_UP, mouseDownButton, 0, 0);
        }
        mouseDownButton = -1;
      }
      if (selecting) {
        stopAutoScroll();
        selecting = false;
        this.selecting = false;
        clearSelection();
      }
    };

    canvas.addEventListener("mousedown", handleMouseDown);
    window.addEventListener("mousemove", handleMouseMove);
    canvas.addEventListener("mousemove", handleHoverMove);
    window.addEventListener("mouseup", handleMouseUp);
    window.addEventListener("blur", handleBlur);
    canvas.addEventListener("wheel", handleWheel, { passive: false });
    canvas.addEventListener("contextmenu", handleContextMenu);
    canvas.addEventListener("click", handleClick);

    this.cleanups.push(() => {
      canvas.removeEventListener("mousedown", handleMouseDown);
      window.removeEventListener("mousemove", handleMouseMove);
      canvas.removeEventListener("mousemove", handleHoverMove);
      window.removeEventListener("mouseup", handleMouseUp);
      window.removeEventListener("blur", handleBlur);
      canvas.removeEventListener("wheel", handleWheel);
      canvas.removeEventListener("contextmenu", handleContextMenu);
      canvas.removeEventListener("click", handleClick);
      if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
      stopAutoScroll();
    });
  }
}
