import type { Terminal } from "@blit-sh/browser";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import type { TerminalPalette, ConnectionStatus, SessionId } from "./types";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import { measureCell, cssFontFamily, type CellMetrics } from "./measure";
import type { GlRenderer } from "./gl-renderer";
import { keyToBytes, ctrlCharToByte, encoder } from "./keyboard";
import { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./protocol";

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export interface BlitTerminalSurfaceOptions {
  sessionId: SessionId | null;
  fontFamily?: string;
  fontSize?: number;
  palette?: TerminalPalette;
  readOnly?: boolean;
  showCursor?: boolean;
  onRender?: (renderMs: number) => void;
  scrollbarColor?: string;
  scrollbarWidth?: number;
  advanceRatio?: number;
}

export interface BlitTerminalSurfaceHandle {
  terminal: Terminal | null;
  rows: number;
  cols: number;
  status: ConnectionStatus;
  focus(): void;
}

// ---------------------------------------------------------------------------
// Internal selection position
// ---------------------------------------------------------------------------

type SelPos = { row: number; col: number; tailOffset: number };

// ---------------------------------------------------------------------------
// DPR detection
// ---------------------------------------------------------------------------

const isSafari =
  typeof navigator !== "undefined"
    ? /^((?!chrome|android).)*safari/i.test(navigator.userAgent)
    : false;

function effectiveDpr(): number {
  if (typeof window === "undefined") return 1;
  const base = window.devicePixelRatio || 1;
  if (isSafari && window.outerWidth && window.innerWidth) {
    const zoom = window.outerWidth / window.innerWidth;
    if (zoom > 0.25 && zoom < 8) return Math.round(base * zoom * 100) / 100;
  }
  return base;
}

// ---------------------------------------------------------------------------
// BlitTerminalSurface
// ---------------------------------------------------------------------------

let surfaceCounter = 0;

/**
 * Framework-agnostic terminal surface. Manages DOM elements, WebGL rendering,
 * keyboard/mouse input, selection, scrollbar, DPR tracking, and resize
 * observation. Framework bindings (React, Solid, etc.) attach this to a
 * container element and forward option changes.
 */
export class BlitTerminalSurface {
  // --- configuration (set via setters) ---
  private _sessionId: SessionId | null = null;
  private _fontFamily: string;
  private _fontSize: number;
  private _palette: TerminalPalette | undefined;
  private _readOnly: boolean;
  private _showCursor: boolean;
  private _onRender: ((renderMs: number) => void) | undefined;
  private _scrollbarColor: string | undefined;
  private _scrollbarWidth: number;
  private _advanceRatio: number | undefined;

  // --- external collaborators ---
  private _workspace: BlitWorkspace | null = null;
  private _blitConn: BlitConnection | null = null;

  // --- DOM elements ---
  private container: HTMLDivElement | null = null;
  private glCanvas: HTMLCanvasElement | null = null;
  private inputEl: HTMLTextAreaElement | null = null;

  // --- mutable state ---
  private viewId: string | null = null;
  private terminal: Terminal | null = null;
  private renderer: GlRenderer | null = null;
  private displayCtx: CanvasRenderingContext2D | null = null;
  private cell: CellMetrics;
  private _rows = 24;
  private _cols = 80;
  private contentDirty = true;
  private lastOffset = 0;
  private lastWasmBuffer: ArrayBuffer | null = null;
  private raf = 0;
  private renderScheduled = false;
  private dpr: number;

  private scrollOffset = 0;
  private scrollFade = 0;
  private scrollFadeTimer: ReturnType<typeof setTimeout> | null = null;
  private scrollbarGeo: {
    barX: number;
    barY: number;
    barW: number;
    barH: number;
    canvasH: number;
    totalLines: number;
    viewportRows: number;
  } | null = null;
  private scrollDragging = false;
  private scrollDragOffset = 0;

  private cursorBlinkOn = true;
  private cursorBlinkTimer: ReturnType<typeof setInterval> | null = null;

  private selStart: SelPos | null = null;
  private selEnd: SelPos | null = null;
  private selecting = false;
  private _selectionListeners = new Set<(hasSelection: boolean) => void>();
  private hoveredUrl: {
    row: number;
    startCol: number;
    endCol: number;
    url: string;
  } | null = null;

  private predicted = "";
  private predictedFromRow = 0;
  private predictedFromCol = 0;

  private wasmReady = false;
  private disposed = false;
  private _ctrlModifier = false;
  private _ctrlModifierListeners = new Set<(active: boolean) => void>();
  private _altModifier = false;
  private _altModifierListeners = new Set<(active: boolean) => void>();

  // --- subscriptions / observers ---
  private dirtyUnsub: (() => void) | null = null;
  private readyUnsub: (() => void) | null = null;
  private resizeObserver: ResizeObserver | null = null;
  private dprMq: MediaQueryList | null = null;
  private dprCheckHandler: (() => void) | null = null;
  private fontsHandler: (() => void) | null = null;

  // --- event handler refs (for cleanup) ---
  private boundKeyDown: ((e: KeyboardEvent) => void) | null = null;
  private boundCompositionEnd: ((e: CompositionEvent) => void) | null = null;
  private boundInput: ((e: Event) => void) | null = null;
  private boundContainerWheel: ((e: WheelEvent) => void) | null = null;
  private mouseCleanup: (() => void) | null = null;
  private windowResizeHandler: (() => void) | null = null;

  constructor(options: BlitTerminalSurfaceOptions) {
    this._sessionId = options.sessionId;
    this._fontFamily = options.fontFamily ?? DEFAULT_FONT;
    this._fontSize = options.fontSize ?? DEFAULT_FONT_SIZE;
    this._palette = options.palette;
    this._readOnly = options.readOnly ?? false;
    this._showCursor = options.showCursor ?? true;
    this._onRender = options.onRender;
    this._scrollbarColor = options.scrollbarColor;
    this._scrollbarWidth = options.scrollbarWidth ?? 4;
    this._advanceRatio = options.advanceRatio;

    this.dpr = effectiveDpr();
    this.cell = measureCell(
      this._fontFamily,
      this._fontSize,
      this.dpr,
      this._advanceRatio,
    );
  }

  // =========================================================================
  // Public API
  // =========================================================================

  get rows(): number {
    return this._rows;
  }

  get cols(): number {
    return this._cols;
  }

  get currentTerminal(): Terminal | null {
    return this.terminal;
  }

  get status(): ConnectionStatus {
    // Reflect the transport's send-readiness, not the post-handshake
    // "authenticating→connected" promotion on S2C_READY.  Input/resize/mouse
    // sends succeed as soon as the transport is connected; gating on the
    // snapshot would block interaction whenever S2C_READY is delayed or lost,
    // even though the server is already accepting C2S_CREATE2 and input.
    return this._blitConn?.transport.status ?? "disconnected";
  }

  focus(): void {
    this.inputEl?.focus();
  }

  /**
   * Set the Ctrl modifier state for the next typed character.
   * When active, the next character typed via the soft keyboard will be
   * converted to its Ctrl+char byte equivalent (e.g. 'c' → Ctrl+C = 0x03).
   * The modifier auto-resets after one character is consumed.
   */
  setCtrlModifier(active: boolean): void {
    if (this._ctrlModifier === active) return;
    this._ctrlModifier = active;
    for (const l of this._ctrlModifierListeners) l(active);
  }

  get ctrlModifier(): boolean {
    return this._ctrlModifier;
  }

  /** Subscribe to Ctrl modifier state changes. Returns unsubscribe function. */
  onCtrlModifierChange(listener: (active: boolean) => void): () => void {
    this._ctrlModifierListeners.add(listener);
    return () => this._ctrlModifierListeners.delete(listener);
  }

  /**
   * Set the Alt modifier state for the next typed character.
   * When active, the next character typed via the soft keyboard will be
   * prefixed with ESC (0x1b), producing an Alt+char sequence.
   * The modifier auto-resets after one character is consumed.
   */
  setAltModifier(active: boolean): void {
    if (this._altModifier === active) return;
    this._altModifier = active;
    for (const l of this._altModifierListeners) l(active);
  }

  get altModifier(): boolean {
    return this._altModifier;
  }

  /** Subscribe to Alt modifier state changes. Returns unsubscribe function. */
  onAltModifierChange(listener: (active: boolean) => void): () => void {
    this._altModifierListeners.add(listener);
    return () => this._altModifierListeners.delete(listener);
  }

  /** True when there is a non-empty active selection on this terminal. */
  hasSelection(): boolean {
    const a = this.selStart;
    const b = this.selEnd;
    if (!a || !b) return false;
    return a.tailOffset !== b.tailOffset || a.col !== b.col;
  }

  /** Subscribe to selection-presence changes. Returns unsubscribe function. */
  onSelectionChange(listener: (hasSelection: boolean) => void): () => void {
    this._selectionListeners.add(listener);
    return () => this._selectionListeners.delete(listener);
  }

  /** Clear any active selection. */
  clearSelection(): void {
    if (!this.selStart && !this.selEnd) return;
    this.selStart = null;
    this.selEnd = null;
    this.scheduleRender();
    this.notifySelectionChange();
  }

  /**
   * Copy the current selection to the clipboard. Returns the copied text,
   * or null when there is no selection or copy is unavailable. Must be
   * invoked from a user gesture (click / pointer / key handler) for
   * `navigator.clipboard.writeText` to succeed in browsers that gate it.
   */
  async copySelection(): Promise<string | null> {
    const ss = this.selStart;
    const se = this.selEnd;
    const t = this.terminal;
    if (!ss || !se || !t) return null;
    let start = ss;
    let end = se;
    // Normalise so start precedes end.
    if (
      start.tailOffset < end.tailOffset ||
      (start.tailOffset === end.tailOffset && start.col > end.col)
    ) {
      [start, end] = [end, start];
    }
    const curScroll = this.scrollOffset;
    const rows = this._rows;
    const startViewRow = rows - 1 - start.tailOffset + curScroll;
    const endViewRow = rows - 1 - end.tailOffset + curScroll;
    const inViewport =
      startViewRow >= 0 &&
      startViewRow < rows &&
      endViewRow >= 0 &&
      endViewRow < rows;
    let text: string | null = null;
    if (inViewport) {
      text = t.get_text(startViewRow, start.col, endViewRow, end.col);
    } else if (
      this._blitConn &&
      this._sessionId !== null &&
      this._blitConn.supportsCopyRange()
    ) {
      try {
        text = await this._blitConn.copyRange(
          this._sessionId,
          start.tailOffset,
          start.col,
          end.tailOffset,
          end.col,
        );
      } catch {
        return null;
      }
    }
    if (!text) return null;
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // Clipboard write rejected (e.g. no permission). Surface the text
      // so callers can fall back to a manual copy affordance.
    }
    return text;
  }

  /**
   * Read text from the system clipboard and send it to the focused
   * session, wrapped in bracketed-paste markers when the terminal is in
   * bracketed-paste mode. Must be invoked from a user gesture for
   * `navigator.clipboard.readText` to succeed in most browsers. Returns
   * the pasted text, or null when nothing is available.
   */
  async pasteFromClipboard(): Promise<string | null> {
    if (this._readOnly) return null;
    if (this._sessionId === null || this.status !== "connected") return null;
    let text: string;
    try {
      text = await navigator.clipboard.readText();
    } catch {
      return null;
    }
    if (!text) return null;
    this.pasteText(text);
    return text;
  }

  /**
   * Send arbitrary text to the focused session as if pasted, wrapped in
   * bracketed-paste markers when the terminal is in bracketed-paste mode.
   * Newlines are normalised to CR so shells that read them as "Enter"
   * behave the same as a desktop paste.
   */
  pasteText(text: string): void {
    if (this._readOnly || !text) return;
    if (this._sessionId === null || this.status !== "connected") return;
    const payload = encoder.encode(text.replace(/\r?\n/g, "\r"));
    const t = this.terminal;
    if (t && t.bracketed_paste()) {
      const open = encoder.encode("\x1b[200~");
      const close = encoder.encode("\x1b[201~");
      const wrapped = new Uint8Array(
        open.length + payload.length + close.length,
      );
      wrapped.set(open, 0);
      wrapped.set(payload, open.length);
      wrapped.set(close, open.length + payload.length);
      this.sendInput(this._sessionId, wrapped);
    } else {
      this.sendInput(this._sessionId, payload);
    }
  }

  private notifySelectionChange(): void {
    const has = this.hasSelection();
    for (const l of this._selectionListeners) l(has);
  }

  /** Attach to a container element. Creates the canvas + textarea inside it. */
  attach(container: HTMLDivElement): void {
    if (this.container === container) return;
    this.detach();
    this.container = container;

    // Create canvas
    this.glCanvas = document.createElement("canvas");
    if (this._readOnly) {
      Object.assign(this.glCanvas.style, {
        display: "block",
        width: "100%",
        height: "100%",
        objectFit: "contain",
        objectPosition: "center",
      });
    } else {
      Object.assign(this.glCanvas.style, {
        display: "block",
        position: "absolute",
        top: "0",
        left: "0",
        cursor: "text",
        // Suppress mobile-browser default touch behaviour so vertical
        // swipes don't scroll the page or trigger pull-to-refresh, and
        // long-press doesn't pop the iOS callout / Android selection
        // handles on top of our own selection rendering.
        touchAction: "none",
        userSelect: "none",
        WebkitUserSelect: "none",
        WebkitTouchCallout: "none",
      });
    }
    container.appendChild(this.glCanvas);

    // Hidden textarea: hosts keyboard focus even in read-only mode so
    // scrollback-navigation keys (Shift+PageUp/PageDown/Home/End) work.
    // In read-only, input-producing event handlers are not wired up in
    // setupKeyboard — only the scroll-key paths run.
    this.inputEl = document.createElement("textarea");
    this.inputEl.setAttribute("aria-label", "Terminal input");
    this.inputEl.setAttribute("autocapitalize", "off");
    this.inputEl.setAttribute("autocomplete", "off");
    this.inputEl.setAttribute("autocorrect", "off");
    this.inputEl.setAttribute("spellcheck", "false");
    this.inputEl.setAttribute("tabindex", "0");
    if (this._readOnly) this.inputEl.setAttribute("readonly", "");
    // Give each textarea a name so browsers don't flag it as an
    // anonymous form field (Chrome DevTools "Issues" warning).
    this.inputEl.setAttribute(
      "name",
      `blit-input-${this._sessionId ?? `anon-${++surfaceCounter}`}`,
    );
    Object.assign(this.inputEl.style, {
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
    container.appendChild(this.inputEl);

    this.setupDprDetection();
    this.setupCursorBlink();
    this.setupRenderer();
    this.setupCellMeasure();
    this.setupTerminal();
    this.setupDirtyListener();
    this.setupResizeObserver();
    this.setupRenderLoop();
    this.setupKeyboard();
    this.setupContainerWheel();
    this.setupMouse();
    this.scheduleRender();
  }

  /** Detach from the current container. Removes all DOM elements and listeners. */
  detach(): void {
    this.teardownMouse();
    this.teardownContainerWheel();
    this.teardownKeyboard();
    this.teardownRenderLoop();
    this.teardownResizeObserver();
    this.teardownDirtyListener();
    this.teardownTerminal();
    this.teardownCellMeasure();
    this.teardownRenderer();
    this.teardownCursorBlink();
    this.teardownDprDetection();

    if (this.glCanvas && this.container?.contains(this.glCanvas)) {
      this.container.removeChild(this.glCanvas);
    }
    if (this.inputEl && this.container?.contains(this.inputEl)) {
      this.container.removeChild(this.inputEl);
    }
    this.glCanvas = null;
    this.inputEl = null;
    this.displayCtx = null;
    this.container = null;
  }

  /** Clean up all resources. Must be called when the surface is no longer needed. */
  dispose(): void {
    this.detach();
    this.disposed = true;
  }

  // --- Setters for configuration ---

  setWorkspace(workspace: BlitWorkspace | null): void {
    this._workspace = workspace;
  }

  setConnection(conn: BlitConnection | null): void {
    if (this._blitConn === conn) return;
    this.teardownDirtyListener();
    this.teardownTerminal();
    this.teardownResizeObserver();
    this.teardownRenderer();
    this._blitConn = conn;
    if (this.container) {
      this.setupRenderer();
      this.setupWasmReady();
      this.setupTerminal();
      this.setupDirtyListener();
      this.setupResizeObserver();
      this.contentDirty = true;
      this.scheduleRender();
    }
  }

  setSessionId(id: SessionId | null): void {
    if (this._sessionId === id) return;
    this.teardownDirtyListener();
    this.teardownTerminal();
    this.teardownResizeObserver();
    this._sessionId = id;
    if (this.container) {
      this.setupTerminal();
      this.setupDirtyListener();
      this.setupResizeObserver();
      this.contentDirty = true;
      this.scheduleRender();
    }
  }

  setPalette(palette: TerminalPalette | undefined): void {
    this._palette = palette;
    this.applyPaletteToTerminal(this.terminal);
  }

  setFontFamily(fontFamily: string | undefined): void {
    const resolved = fontFamily ?? DEFAULT_FONT;
    if (this._fontFamily === resolved) return;
    this._fontFamily = resolved;
    this.remeasureCells(true);
  }

  setFontSize(fontSize: number | undefined): void {
    const resolved = fontSize ?? DEFAULT_FONT_SIZE;
    if (this._fontSize === resolved) return;
    this._fontSize = resolved;
    this.remeasureCells(true);
  }

  /**
   * Update the read-only flag. Note: this only takes full effect when set
   * before `attach()`. Changing it while attached will not create/remove the
   * input textarea or toggle keyboard/mouse listeners.
   */
  /**
   * Update the read-only flag. Note: this only takes full effect when set
   * before `attach()`. Changing it while attached will not create/remove the
   * input textarea or toggle keyboard/mouse listeners.
   */
  setReadOnly(readOnly: boolean | undefined): void {
    this._readOnly = readOnly ?? false;
  }

  setShowCursor(show: boolean | undefined): void {
    const resolved = show ?? true;
    if (this._showCursor === resolved) return;
    this._showCursor = resolved;
    this.contentDirty = true;
    this.scheduleRender();
  }

  setOnRender(fn: ((renderMs: number) => void) | undefined): void {
    this._onRender = fn;
  }

  setAdvanceRatio(ratio: number | undefined): void {
    if (this._advanceRatio === ratio) return;
    this._advanceRatio = ratio;
    this.remeasureCells(true);
  }

  // =========================================================================
  // Private setup/teardown methods
  // =========================================================================

  private scheduleRender(): void {
    if (this.renderScheduled || this.disposed) return;
    this.renderScheduled = true;
    this.raf = requestAnimationFrame(() => {
      this.renderScheduled = false;
      this.doRender();
    });
  }

  // --- DPR detection ---

  private setupDprDetection(): void {
    this.dprCheckHandler = () => {
      const next = effectiveDpr();
      if (next !== this.dpr) {
        this.dpr = next;
        this.remeasureCells(true);
      }
    };
    if (typeof window.matchMedia === "function") {
      this.dprMq = window.matchMedia(
        `(resolution: ${window.devicePixelRatio}dppx)`,
      );
      this.dprMq.addEventListener("change", this.dprCheckHandler);
    }
    window.addEventListener("resize", this.dprCheckHandler);
  }

  private teardownDprDetection(): void {
    if (this.dprCheckHandler) {
      this.dprMq?.removeEventListener("change", this.dprCheckHandler);
      window.removeEventListener("resize", this.dprCheckHandler);
      this.dprCheckHandler = null;
      this.dprMq = null;
    }
  }

  // --- Cell measurement ---

  private setupCellMeasure(): void {
    this.remeasureCells(true);
    this.fontsHandler = () => this.remeasureCells(true);
    document.fonts?.addEventListener("loadingdone", this.fontsHandler);
    if (document.fonts?.status === "loaded") this.remeasureCells(true);
  }

  private teardownCellMeasure(): void {
    if (this.fontsHandler) {
      document.fonts?.removeEventListener("loadingdone", this.fontsHandler);
      this.fontsHandler = null;
    }
  }

  private remeasureCells(forceInvalidate = false): void {
    const cell = measureCell(
      this._fontFamily,
      this._fontSize,
      this.dpr,
      this._advanceRatio,
    );
    const changed = cell.pw !== this.cell.pw || cell.ph !== this.cell.ph;
    const shouldInvalidate = forceInvalidate || changed;
    this.cell = cell;

    const rasterFontSize = this._fontSize * this.dpr;
    if (!this._readOnly) {
      const t = this.terminal;
      if (t) {
        t.set_cell_size(cell.pw, cell.ph);
        t.set_font_family(this._fontFamily);
        t.set_font_size(rasterFontSize);
        if (shouldInvalidate) t.invalidate_render_cache();
      }
      if (this._blitConn) {
        this._blitConn.setCellSize(cell.pw, cell.ph);
        this._blitConn.setFontFamily(this._fontFamily);
        this._blitConn.setFontSize(rasterFontSize);
      }
    }
    if (shouldInvalidate) {
      this.contentDirty = true;
      this.scheduleRender();
    }
    if (changed) {
      this.handleResize();
    }
  }

  // --- Cursor blink ---

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

  // --- GL renderer ---

  private setupRenderer(): void {
    if (!this._blitConn) return;
    const shared = this._blitConn.getSharedRenderer();
    if (shared) this.renderer = shared.renderer;
  }

  private teardownRenderer(): void {
    // renderer is shared, don't dispose
    this.renderer = null;
  }

  // --- WASM ready ---

  private setupWasmReady(): void {
    this.readyUnsub?.();
    this.readyUnsub = null;
    if (!this._blitConn) {
      this.wasmReady = false;
      return;
    }
    this.readyUnsub = this._blitConn.onReady(() => {
      this.wasmReady = true;
    });
    if (this._blitConn.isReady()) this.wasmReady = true;
  }

  // --- Terminal lifecycle ---

  private setupTerminal(): void {
    if (!this._blitConn) {
      this.terminal = null;
      return;
    }
    this.setupWasmReady();
    if (this._sessionId !== null) {
      this._blitConn.retain(this._sessionId);
      const t = this._blitConn.getTerminal(this._sessionId);
      if (t) {
        this.terminal = t;
        this.applyPaletteToTerminal(t);
        if (!this._readOnly) {
          t.set_cell_size(this.cell.pw, this.cell.ph);
          t.set_font_family(this._fontFamily);
          t.set_font_size(this._fontSize * this.dpr);
        }
        this.contentDirty = true;
        this.scheduleRender();
      }
    } else {
      this.terminal = null;
    }
  }

  private teardownTerminal(): void {
    this.terminal = null;
    if (this._sessionId !== null && this._blitConn) {
      this._blitConn.release(this._sessionId);
    }
    this.readyUnsub?.();
    this.readyUnsub = null;
  }

  // --- Dirty listener ---

  private setupDirtyListener(): void {
    if (!this._blitConn || this._sessionId === null) return;
    const conn = this._blitConn;
    const sessionId = this._sessionId;
    this.dirtyUnsub = conn.addDirtyListener(sessionId, () => {
      const t = conn.getTerminal(sessionId);
      if (!t) return;
      if (this.terminal !== t) {
        this.terminal = t;
        this.applyPaletteToTerminal(t);
        this.applyMetricsToTerminal(t);
      }
      this.contentDirty = true;
      this.scheduleRender();
      this.reconcilePrediction();
      if (this._readOnly) this.syncReadOnlySize(t);
    });
    // Check for terminal that was created between setup steps.
    const t = conn.getTerminal(sessionId);
    if (t) {
      if (this.terminal !== t) {
        this.terminal = t;
        this.applyPaletteToTerminal(t);
        this.applyMetricsToTerminal(t);
      }
      this.contentDirty = true;
      this.scheduleRender();
      if (this._readOnly) this.syncReadOnlySize(t);
    }
  }

  private teardownDirtyListener(): void {
    this.dirtyUnsub?.();
    this.dirtyUnsub = null;
  }

  // --- Palette ---

  private applyPaletteToTerminal(t: Terminal | null): void {
    if (!t || !this._palette) return;
    t.set_default_colors(...this._palette.fg, ...this._palette.bg);
    for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...this._palette.ansi[i]);
    this.contentDirty = true;
    this.scheduleRender();
  }

  private applyMetricsToTerminal(t: Terminal): void {
    t.set_cell_size(this.cell.pw, this.cell.ph);
    t.set_font_family(this._fontFamily);
    t.set_font_size(this._fontSize * this.dpr);
    t.invalidate_render_cache();
  }

  private syncReadOnlySize(t: Terminal): void {
    const tr = t.rows;
    const tc = t.cols;
    if (tr !== this._rows || tc !== this._cols) {
      this._rows = tr;
      this._cols = tc;
    }
    this.scheduleRender();
  }

  // --- Resize observer ---

  private setupResizeObserver(): void {
    if (!this.container || this._readOnly) return;

    if (!this.viewId && this._blitConn) {
      this.viewId = this._blitConn.allocViewId();
    }

    this.windowResizeHandler = () => this.handleResize();
    this.resizeObserver = new ResizeObserver(() => this.handleResize());
    this.resizeObserver.observe(this.container);
    window.addEventListener("resize", this.windowResizeHandler);
    this.handleResize(true /* immediate */);
  }

  private teardownResizeObserver(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    clearTimeout(this._resizeTimer);
    if (this.windowResizeHandler) {
      window.removeEventListener("resize", this.windowResizeHandler);
      this.windowResizeHandler = null;
    }
    if (this._sessionId !== null && this._blitConn && this.viewId) {
      this._blitConn.removeView(this._sessionId, this.viewId);
    }
  }

  private _resizeTimer: ReturnType<typeof setTimeout> | undefined;

  private handleResize(immediate?: boolean): void {
    if (!this.container || this._readOnly) return;
    const w = this.container.clientWidth;
    const h = this.container.clientHeight;
    const cols = Math.max(1, Math.floor(w / this.cell.w));
    const rows = Math.max(1, Math.floor(h / this.cell.h));
    const sizeChanged = cols !== this._cols || rows !== this._rows;
    if (sizeChanged) {
      this._rows = rows;
      this._cols = cols;
      // Debounce the server notification to avoid flooding the server
      // with intermediate sizes during drag-resize, which causes
      // expensive encoder recreation cycles for h264-software.
      // Render locally is immediate; only the network message is delayed.
      if (this._sessionId !== null && this._blitConn && this.viewId) {
        if (immediate) {
          this._blitConn.setViewSize(this._sessionId, this.viewId, rows, cols);
        } else {
          clearTimeout(this._resizeTimer);
          this._resizeTimer = setTimeout(() => {
            if (this._sessionId !== null && this._blitConn && this.viewId) {
              this._blitConn.setViewSize(
                this._sessionId,
                this.viewId,
                rows,
                cols,
              );
            }
          }, 150);
        }
      }
    }
    this.contentDirty = true;
    this.scheduleRender();
  }

  /** Re-send dimensions when connection becomes ready. */
  resendSize(): void {
    if (
      this._sessionId !== null &&
      !this._readOnly &&
      this._blitConn &&
      this.viewId &&
      this._rows > 0 &&
      this._cols > 0
    ) {
      this._blitConn.setViewSize(
        this._sessionId,
        this.viewId,
        this._rows,
        this._cols,
      );
    }
  }

  // --- Render loop ---

  private setupRenderLoop(): void {
    this.scheduleRender();
  }

  private teardownRenderLoop(): void {
    cancelAnimationFrame(this.raf);
    this.renderScheduled = false;
  }

  private doRender(): void {
    const t0 = performance.now();
    const conn = this._blitConn;
    if (!conn) return;

    if (!this.renderer?.supported) {
      const shared = conn.getSharedRenderer();
      if (shared) this.renderer = shared.renderer;
      if (!this.renderer?.supported) {
        conn.noteFrameRendered();
        return;
      }
    }
    if (!this.terminal) {
      conn.noteFrameRendered();
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
      const glCanvas = this.glCanvas;
      if (glCanvas) {
        if (glCanvas.style.width !== cssW) glCanvas.style.width = cssW;
        if (glCanvas.style.height !== cssH) glCanvas.style.height = cssH;
      }
    }

    const mem = conn.wasmMemory();
    if (!mem) {
      conn.noteFrameRendered();
      return;
    }
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

    const bgVerts = new Float32Array(
      mem.buffer,
      t.bg_verts_ptr(),
      t.bg_verts_len(),
    );
    const glyphVerts = new Float32Array(
      mem.buffer,
      t.glyph_verts_ptr(),
      t.glyph_verts_len(),
    );
    renderer.resize(pw, ph);
    const predictedLen = this.predicted.length;
    let effectiveCursorCol = t.cursor_col;
    let effectiveCursorRow = t.cursor_row;
    if (predictedLen > 0 && this._cols > 0) {
      const abs = t.cursor_col + predictedLen;
      effectiveCursorCol = abs % this._cols;
      effectiveCursorRow = Math.min(
        t.cursor_row + Math.floor(abs / this._cols),
        this._rows - 1,
      );
    }
    renderer.render(
      bgVerts,
      glyphVerts,
      t.glyph_atlas_canvas(),
      t.glyph_atlas_version(),
      t.cursor_visible(),
      effectiveCursorCol,
      effectiveCursorRow,
      t.cursor_style(),
      this.cursorBlinkOn,
      cell,
      this._palette?.bg ?? [0, 0, 0],
      this._showCursor,
    );

    // Copy GL to display canvas, then draw overlay content on top.
    const shared = conn.getSharedRenderer();
    const displayCanvas = this.glCanvas;
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
        this.drawSelectionOverlay(ctx, cell);
        this.drawUrlOverlay(ctx, cell);
        this.drawOverflowText(ctx, t, cell);
        this.drawPredictedEcho(ctx, t, cell);
        this.drawScrollbar(ctx, t, cell);
      }
    }

    // Notify flow control in all modes — the server paces on
    // `pendingAppliedFrames` / `ackAheadFrames`, and suppressing this
    // call in read-only lets those counters climb to 0xffff, which the
    // server reads as "client is completely backlogged" and throttles
    // updates to a crawl.
    conn.noteFrameRendered();
    this._onRender?.(performance.now() - t0);
  }

  // --- Overlay drawing helpers ---

  private drawSelectionOverlay(
    ctx: CanvasRenderingContext2D,
    cell: CellMetrics,
  ): void {
    const ss = this.selStart;
    const se = this.selEnd;
    if (!ss || !se) return;
    const curScroll = this.scrollOffset;
    const rows = this._rows;
    const toViewRow = (p: SelPos) => rows - 1 - p.tailOffset + curScroll;
    let sr = toViewRow(ss),
      sc = ss.col;
    let er = toViewRow(se),
      ec = se.col;
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

  private drawUrlOverlay(
    ctx: CanvasRenderingContext2D,
    cell: CellMetrics,
  ): void {
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

  private drawOverflowText(
    ctx: CanvasRenderingContext2D,
    t: Terminal,
    cell: CellMetrics,
  ): void {
    const overflowCount = t.overflow_text_count();
    if (overflowCount <= 0) return;
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

  private drawPredictedEcho(
    ctx: CanvasRenderingContext2D,
    t: Terminal,
    cell: CellMetrics,
  ): void {
    if (this._readOnly || !this.predicted) return;
    if (!t.echo()) return;
    const cw = cell.pw;
    const ch = cell.ph;
    const [fR, fG, fB] = this._palette?.fg ?? [204, 204, 204];
    ctx.fillStyle = `rgba(${fR},${fG},${fB},0.5)`;
    const fSize = Math.max(1, Math.round(ch * 0.85));
    ctx.font = `${fSize}px ${cssFontFamily(this._fontFamily)}`;
    ctx.textBaseline = "bottom";
    const cc = t.cursor_col;
    const cr = t.cursor_row;
    for (let i = 0; i < this.predicted.length && cc + i < this._cols; i++) {
      ctx.fillText(this.predicted[i], (cc + i) * cw, cr * ch + ch);
    }
  }

  private drawScrollbar(
    ctx: CanvasRenderingContext2D,
    t: Terminal,
    cell: CellMetrics,
  ): void {
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
    this.scrollbarGeo = {
      barX,
      barY,
      barW,
      barH,
      canvasH,
      totalLines,
      viewportRows,
    };
    const show =
      this.scrollFade > 0 || this.scrollDragging || this.scrollOffset > 0;
    if (show) {
      if (this._scrollbarColor) {
        ctx.fillStyle = this._scrollbarColor;
      } else {
        const [r, g, b] = this._palette?.fg ?? [204, 204, 204];
        ctx.fillStyle = `rgba(${r},${g},${b},0.35)`;
      }
      ctx.beginPath();
      ctx.roundRect(barX, barY, barW, barH, barW / 2);
      ctx.fill();
    }
  }

  // --- Prediction ---

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

  // --- Keyboard ---

  private setupKeyboard(): void {
    const input = this.inputEl;
    if (!input) return;

    this.boundKeyDown = (e: KeyboardEvent) => {
      if (e.defaultPrevented) return;
      if (this._sessionId === null || this.status !== "connected") return;
      if (e.isComposing) return;
      if (e.key === "Dead") return;

      // Scroll-key shortcuts run in all modes, including read-only.
      if (e.shiftKey && (e.key === "PageUp" || e.key === "PageDown")) {
        const t2 = this.terminal;
        const maxScroll = t2 ? t2.scrollback_lines() : 0;
        if (maxScroll > 0 || this.scrollOffset > 0) {
          e.preventDefault();
          const delta = e.key === "PageUp" ? this._rows : -this._rows;
          this.scrollOffset = Math.max(
            0,
            Math.min(maxScroll, this.scrollOffset + delta),
          );
          this.sendScroll(this._sessionId!, this.scrollOffset);
          this.flashScrollbar();
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
          this.sendScroll(this._sessionId!, this.scrollOffset);
          this.flashScrollbar();
          this.scheduleRender();
        }
        return;
      }

      // Past this point: input-producing paths, blocked in read-only.
      if (this._readOnly) return;

      // Ctrl modifier from mobile toolbar: intercept the next printable key
      if (
        this._ctrlModifier &&
        e.key.length === 1 &&
        !e.ctrlKey &&
        !e.metaKey
      ) {
        const bytes = ctrlCharToByte(e.key);
        if (bytes) {
          e.preventDefault();
          this.sendInput(this._sessionId!, bytes);
        }
        this.setCtrlModifier(false);
        return;
      }

      // Alt modifier from mobile toolbar: prefix next printable key with ESC
      if (
        this._altModifier &&
        e.key.length === 1 &&
        !e.ctrlKey &&
        !e.metaKey &&
        !e.altKey
      ) {
        e.preventDefault();
        const charCode = e.key.charCodeAt(0);
        this.sendInput(this._sessionId!, new Uint8Array([0x1b, charCode]));
        this.setAltModifier(false);
        return;
      }

      const t = this.terminal;
      const appCursor = t ? t.app_cursor() : false;
      const bytes = keyToBytes(e, appCursor);
      if (bytes) {
        e.preventDefault();
        if (this.scrollOffset > 0) {
          this.scrollOffset = 0;
          this.sendScroll(this._sessionId!, 0);
        }
        if (
          t &&
          t.echo() &&
          e.key.length === 1 &&
          !e.ctrlKey &&
          !e.metaKey &&
          !e.altKey
        ) {
          if (!this.predicted) {
            this.predictedFromRow = t.cursor_row;
            this.predictedFromCol = t.cursor_col;
          }
          this.predicted += e.key;
          this.scheduleRender();
        } else {
          this.predicted = "";
        }
        this.sendInput(this._sessionId!, bytes);
      }
    };

    if (this._readOnly) {
      input.addEventListener("keydown", this.boundKeyDown);
      return;
    }

    this.boundCompositionEnd = (e: CompositionEvent) => {
      if (e.data && this._sessionId !== null && this.status === "connected") {
        this.sendInput(this._sessionId, encoder.encode(e.data));
      }
      input.value = "";
    };

    this.boundInput = (e: Event) => {
      const inputEvent = e as InputEvent;
      if (inputEvent.isComposing) {
        if (
          inputEvent.inputType === "deleteContentBackward" &&
          !input.value &&
          this._sessionId !== null &&
          this.status === "connected"
        ) {
          this.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
        return;
      }
      // Ctrl modifier: convert the next typed character to Ctrl+char
      if (
        this._ctrlModifier &&
        input.value &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const char = input.value[0];
        const bytes = ctrlCharToByte(char);
        if (bytes) {
          this.sendInput(this._sessionId, bytes);
        }
        this.setCtrlModifier(false);
        input.value = "";
        return;
      }
      // Alt modifier: prefix next typed character with ESC
      if (
        this._altModifier &&
        input.value &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const char = input.value[0];
        const charCode = char.charCodeAt(0);
        this.sendInput(this._sessionId, new Uint8Array([0x1b, charCode]));
        this.setAltModifier(false);
        input.value = "";
        return;
      }
      if (inputEvent.inputType === "deleteContentBackward" && !input.value) {
        if (this._sessionId !== null && this.status === "connected") {
          this.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
      } else if (
        input.value &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const payload = encoder.encode(input.value.replace(/\n/g, "\r"));
        const isPaste = inputEvent.inputType === "insertFromPaste";
        const t = this.terminal;
        if (isPaste && t && t.bracketed_paste()) {
          const open = encoder.encode("\x1b[200~");
          const close = encoder.encode("\x1b[201~");
          const wrapped = new Uint8Array(
            open.length + payload.length + close.length,
          );
          wrapped.set(open, 0);
          wrapped.set(payload, open.length);
          wrapped.set(close, open.length + payload.length);
          this.sendInput(this._sessionId, wrapped);
        } else {
          this.sendInput(this._sessionId, payload);
        }
      }
      input.value = "";
    };

    input.addEventListener("keydown", this.boundKeyDown);
    input.addEventListener("compositionend", this.boundCompositionEnd);
    input.addEventListener("input", this.boundInput);
  }

  private teardownKeyboard(): void {
    const input = this.inputEl;
    if (!input) return;
    if (this.boundKeyDown)
      input.removeEventListener("keydown", this.boundKeyDown);
    if (this.boundCompositionEnd)
      input.removeEventListener("compositionend", this.boundCompositionEnd);
    if (this.boundInput) input.removeEventListener("input", this.boundInput);
    this.boundKeyDown = null;
    this.boundCompositionEnd = null;
    this.boundInput = null;
  }

  // --- Container wheel ---

  private setupContainerWheel(): void {
    if (!this.container) return;
    this.boundContainerWheel = (e: WheelEvent) => {
      const t = this.terminal;
      // In read-only, always do scrollback — we never forward wheel as
      // mouse events to the host shell.  In read/write, let the canvas
      // wheel handler (setupMouse) take the event when the PTY is in
      // mouse-reporting mode, unless shift is held.
      if (!this._readOnly && t && t.mouse_mode() > 0 && !e.shiftKey) return;
      if (this._sessionId !== null && this.status === "connected") {
        const maxScroll = t ? t.scrollback_lines() : 0;
        if (maxScroll === 0 && this.scrollOffset === 0) return;
        e.preventDefault();
        const delta =
          Math.abs(e.deltaY) > Math.abs(e.deltaX) ? e.deltaY : e.deltaX;
        const lines = Math.round(-delta / 20) || (delta > 0 ? -3 : 3);
        this.scrollOffset = Math.max(
          0,
          Math.min(maxScroll, this.scrollOffset + lines),
        );
        this.sendScroll(this._sessionId!, this.scrollOffset);
        if (this.scrollOffset > 0) this.flashScrollbar();
        this.scheduleRender();
      }
    };
    this.container.addEventListener("wheel", this.boundContainerWheel, {
      passive: false,
    });
  }

  private teardownContainerWheel(): void {
    if (this.boundContainerWheel && this.container) {
      this.container.removeEventListener("wheel", this.boundContainerWheel);
    }
    this.boundContainerWheel = null;
  }

  // --- Mouse input ---

  private setupMouse(): void {
    const canvas = this.glCanvas;
    if (!canvas || this._readOnly) return;

    const SCROLLBAR_HIT_PX = 20;
    const WORD_CHARS = /[A-Za-z0-9_\-./~:@]/;
    const URL_RE = /https?:\/\/[^\s<>"'`)\]},;]+/g;
    const AUTO_SCROLL_INTERVAL_MS = 50;
    const AUTO_SCROLL_LINES = 3;

    let mouseDownButton = -1;
    let lastMouseCell = { row: -1, col: -1 };
    let selecting = false;
    let selGranularity: 1 | 2 | 3 = 1;
    let selAnchorStart: SelPos | null = null;
    let selAnchorEnd: SelPos | null = null;
    let autoScrollTimer: ReturnType<typeof setInterval> | null = null;
    let autoScrollDir: -1 | 0 | 1 = 0;
    let lastHoverUrl: string | null = null;

    const mouseToCell = (e: MouseEvent) => {
      const rect = canvas.getBoundingClientRect();
      return {
        row: Math.min(
          Math.max(Math.floor((e.clientY - rect.top) / this.cell.h), 0),
          this._rows - 1,
        ),
        col: Math.min(
          Math.max(Math.floor((e.clientX - rect.left) / this.cell.w), 0),
          this._cols - 1,
        ),
      };
    };

    const canvasYFromEvent = (e: MouseEvent) => {
      const rect = canvas.getBoundingClientRect();
      const dpr = this.cell.pw / this.cell.w;
      return (e.clientY - rect.top) * dpr;
    };

    const isNearScrollbar = (e: MouseEvent) => {
      const rect = canvas.getBoundingClientRect();
      return e.clientX >= rect.right - SCROLLBAR_HIT_PX;
    };

    const scrollToCanvasY = (y: number) => {
      const geo = this.scrollbarGeo;
      if (!geo || this._sessionId === null || this.status !== "connected")
        return;
      const fraction = 1 - y / (geo.canvasH - geo.barH);
      const maxScroll = geo.totalLines - geo.viewportRows;
      const offset = Math.round(
        Math.max(0, Math.min(maxScroll, fraction * maxScroll)),
      );
      this.scrollOffset = offset;
      this.sendScroll(this._sessionId!, offset);
      this.scrollFade = 1;
      this.scheduleRender();
    };

    const sendMouseEvent = (
      type: "down" | "up" | "move",
      e: MouseEvent,
      button: number,
    ): boolean => {
      if (this._sessionId === null || this.status !== "connected") return false;
      const t = this.terminal;
      if (t && t.mouse_mode() === 0) return false;
      const pos = mouseToCell(e);
      const typeCode =
        type === "down" ? MOUSE_DOWN : type === "up" ? MOUSE_UP : MOUSE_MOVE;
      this._workspace?.sendMouse(
        this._sessionId!,
        typeCode,
        button,
        pos.col,
        pos.row,
      );
      return true;
    };

    const cellToSel = (cell: { row: number; col: number }): SelPos => ({
      row: cell.row,
      col: cell.col,
      tailOffset: this.scrollOffset + (this._rows - 1 - cell.row),
    });

    const stopAutoScroll = () => {
      if (autoScrollTimer !== null) {
        clearInterval(autoScrollTimer);
        autoScrollTimer = null;
      }
      autoScrollDir = 0;
    };

    const getRowText = (row: number): string => {
      const t = this.terminal;
      return t ? t.get_text(row, 0, row, this._cols - 1) : "";
    };

    const getRowColMap = (row: number): Uint16Array | null => {
      const t = this.terminal;
      return t ? t.row_col_map(row) : null;
    };

    const colToTextIdx = (colMap: Uint16Array, col: number): number => {
      for (let i = 0; i < colMap.length; i++) {
        if (colMap[i] === col) return i;
      }
      return -1;
    };

    const wordBoundsAt = (row: number, col: number) => {
      const text = getRowText(row);
      const colMap = getRowColMap(row);
      const idx = colMap ? colToTextIdx(colMap, col) : col;
      if (idx < 0 || idx >= text.length || !WORD_CHARS.test(text[idx]))
        return { start: col, end: col };
      let start = idx;
      while (start > 0 && WORD_CHARS.test(text[start - 1])) start--;
      let end = idx;
      while (end < text.length - 1 && WORD_CHARS.test(text[end + 1])) end++;
      const startCol = colMap ? (colMap[start] ?? start) : start;
      const endCol = colMap ? (colMap[end] ?? end) : end;
      return { start: startCol, end: endCol };
    };

    const isWrapped = (row: number): boolean => {
      const t = this.terminal;
      return t ? t.is_wrapped(row) : false;
    };

    const logicalLineRange = (row: number) => {
      const maxRow = this._rows - 1;
      let startRow = row;
      while (startRow > 0 && isWrapped(startRow - 1)) startRow--;
      let endRow = row;
      while (endRow < maxRow && isWrapped(endRow)) endRow++;
      return { startRow, endRow };
    };

    const applyGranularity = (cell: { row: number; col: number }) => {
      if (selGranularity === 3) {
        const { startRow, endRow } = logicalLineRange(cell.row);
        return {
          start: { row: startRow, col: 0 },
          end: { row: endRow, col: this._cols - 1 },
        };
      }
      if (selGranularity === 2) {
        const wb = wordBoundsAt(cell.row, cell.col);
        return {
          start: { row: cell.row, col: wb.start },
          end: { row: cell.row, col: wb.end },
        };
      }
      return { start: cell, end: cell };
    };

    const applyGranularitySel = (pos: SelPos) => {
      const curScroll = this.scrollOffset;
      const viewRow = this._rows - 1 - pos.tailOffset + curScroll;
      const cell = { row: viewRow, col: pos.col };
      const { start, end } = applyGranularity(cell);
      return {
        start: {
          ...start,
          tailOffset: curScroll + (this._rows - 1 - start.row),
        },
        end: {
          ...end,
          tailOffset: curScroll + (this._rows - 1 - end.row),
        },
      };
    };

    const selPosBefore = (a: SelPos, b: SelPos): boolean =>
      a.tailOffset > b.tailOffset ||
      (a.tailOffset === b.tailOffset && a.col < b.col);

    const startAutoScroll = (dir: -1 | 1) => {
      if (autoScrollDir === dir && autoScrollTimer !== null) return;
      stopAutoScroll();
      autoScrollDir = dir;
      autoScrollTimer = setInterval(() => {
        if (
          !selecting ||
          this._sessionId === null ||
          this.status !== "connected"
        ) {
          stopAutoScroll();
          return;
        }
        const t = this.terminal;
        if (!t) return;
        const maxScroll = t.scrollback_lines();
        const prev = this.scrollOffset;
        const next = Math.max(
          0,
          Math.min(maxScroll, prev + dir * AUTO_SCROLL_LINES),
        );
        if (next === prev) return;
        this.scrollOffset = next;
        this.sendScroll(this._sessionId!, next);
        this.flashScrollbar();
        const edgeRow = dir === 1 ? 0 : this._rows - 1;
        const edgeCol = dir === 1 ? 0 : this._cols - 1;
        const edgeSel = cellToSel({ row: edgeRow, col: edgeCol });
        if (selGranularity >= 2 && selAnchorStart && selAnchorEnd) {
          const { start: dragStart, end: dragEnd } =
            applyGranularitySel(edgeSel);
          if (selPosBefore(dragStart, selAnchorStart)) {
            this.selStart = dragStart;
            this.selEnd = selAnchorEnd;
          } else {
            this.selStart = selAnchorStart;
            this.selEnd = dragEnd;
          }
        } else {
          this.selEnd = edgeSel;
        }
        this.scheduleRender();
      }, AUTO_SCROLL_INTERVAL_MS);
    };

    const clearSelection = () => {
      this.clearSelection();
    };

    const copySelection = () => {
      // Public copySelection() is async but mouse handlers don't await; the
      // copy still happens within the user gesture's microtask, which is
      // sufficient for clipboard permission in browsers that gate it.
      void this.copySelection();
    };

    const urlAt = (row: number, col: number) => {
      const text = getRowText(row);
      const colMap = getRowColMap(row);
      URL_RE.lastIndex = 0;
      let m: RegExpExecArray | null;
      while ((m = URL_RE.exec(text)) !== null) {
        const raw = m[0].replace(/[.),:;]+$/, "");
        const startCol = colMap ? (colMap[m.index] ?? m.index) : m.index;
        const endIdx = m.index + raw.length - 1;
        const endCol = colMap ? (colMap[endIdx] ?? endIdx) : endIdx;
        if (col >= startCol && col <= endCol)
          return { url: raw, startCol, endCol };
      }
      return null;
    };

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
        e.preventDefault();
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
          this.scheduleRender();
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
      const overCanvas =
        mouseDownButton >= 0 || canvas.contains(e.target as Node);
      if (!e.shiftKey && overCanvas) {
        const t = this.terminal;
        if (t) {
          const mode = t.mouse_mode();
          if (mode >= 3) {
            const cell = mouseToCell(e);
            if (
              cell.row === lastMouseCell.row &&
              cell.col === lastMouseCell.col
            )
              return;
            lastMouseCell = cell;
            if (e.buttons) {
              const button =
                e.buttons & 1 ? 0 : e.buttons & 2 ? 2 : e.buttons & 4 ? 1 : 0;
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
          if (selPosBefore(dragStart, selAnchorStart)) {
            this.selStart = dragStart;
            this.selEnd = selAnchorEnd;
          } else {
            this.selStart = selAnchorStart;
            this.selEnd = dragEnd;
          }
        } else {
          this.selEnd = sel;
        }
        this.scheduleRender();
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
        if (selGranularity === 1) this.selEnd = cellToSel(mouseToCell(e));
        this.scheduleRender();
        if (
          this.selStart &&
          this.selEnd &&
          (this.selStart.tailOffset !== this.selEnd.tailOffset ||
            this.selStart.col !== this.selEnd.col)
        ) {
          copySelection();
        }
        clearSelection();
      }
      if (canvas.contains(e.target as Node)) {
        this.inputEl?.focus();
      }
    };

    const handleCanvasWheel = (e: WheelEvent) => {
      const t = this.terminal;
      if (t && t.mouse_mode() > 0 && !e.shiftKey) {
        e.preventDefault();
        const button = e.deltaY < 0 ? 64 : 65;
        sendMouseEvent("down", e, button);
      }
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
      this.inputEl?.focus();
    };

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
        this.hoveredUrl = hit
          ? {
              row: cell.row,
              startCol: hit.startCol,
              endCol: hit.endCol,
              url: hit.url,
            }
          : null;
        this.scheduleRender();
      }
    };

    const handleBlur = () => {
      if (mouseDownButton >= 0) {
        if (this._sessionId !== null && this.status === "connected") {
          this._workspace?.sendMouse(
            this._sessionId,
            MOUSE_UP,
            mouseDownButton,
            0,
            0,
          );
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

    // --- Touch-based scrolling and selection (mobile) ---
    // On mobile, vertical swipes don't reliably produce wheel events.
    // Track single-finger vertical movement and translate into scroll
    // events (mouse-mode wheel buttons or scrollback navigation).
    //
    // Long-press also enters a selection mode so users can pick text
    // without a physical pointer:
    //   * Tap and hold ~500ms — start selecting at the touched word.
    //   * Drag — extend selection toward the finger.
    //   * Lift — selection persists; the mobile toolbar exposes Copy.
    //   * Tap elsewhere — clear the selection.
    const LONG_PRESS_MS = 500;
    const LONG_PRESS_SLOP_PX = 8;
    let touchId: number | null = null;
    let touchStartX = 0;
    let touchStartY = 0;
    let touchLastY = 0;
    let touchAccum = 0;
    let longPressTimer: ReturnType<typeof setTimeout> | null = null;
    let touchSelecting = false;
    let touchSelAnchor: SelPos | null = null;
    let touchScrolled = false;

    const cancelLongPress = () => {
      if (longPressTimer !== null) {
        clearTimeout(longPressTimer);
        longPressTimer = null;
      }
    };

    const startTouchSelection = (clientX: number, clientY: number) => {
      // Cancel any in-flight mouse selection and seed a fresh anchor at the
      // tapped word. The selection persists past touchend so the user can
      // act on it from the mobile toolbar.
      this.clearSelection();
      const cell = mouseToCell(new MouseEvent("touch", { clientX, clientY }));
      const sel = cellToSel(cell);
      const wb = wordBoundsAt(cell.row, cell.col);
      const start: SelPos = {
        row: cell.row,
        col: wb.start,
        tailOffset: this.scrollOffset + (this._rows - 1 - cell.row),
      };
      const end: SelPos = {
        row: cell.row,
        col: wb.end,
        tailOffset: this.scrollOffset + (this._rows - 1 - cell.row),
      };
      this.selStart = start;
      this.selEnd = end;
      touchSelAnchor = sel;
      touchSelecting = true;
      this.scheduleRender();
      this.notifySelectionChange();
      // Haptic nudge if the platform supports it.
      navigator.vibrate?.(15);
    };

    const handleTouchStart = (e: TouchEvent) => {
      if (e.touches.length !== 1) {
        // A second finger arrived — abort any pending long-press and any
        // in-progress touch selection so the user can pinch/zoom or use
        // multi-touch gestures without dragging the selection along.
        cancelLongPress();
        if (touchSelecting) {
          touchSelecting = false;
          touchSelAnchor = null;
        }
        return;
      }
      const touch = e.touches[0]!;
      // If the user taps while a selection is showing, treat it as
      // "dismiss" — but only when the tap doesn't land inside the
      // existing selection rectangle. Tapping inside is reserved for
      // future drag-handle work; for now, also dismiss.
      if (this.hasSelection() && !touchSelecting) {
        this.clearSelection();
      }
      touchId = touch.identifier;
      touchStartX = touch.clientX;
      touchStartY = touch.clientY;
      touchLastY = touch.clientY;
      touchAccum = 0;
      touchScrolled = false;
      cancelLongPress();
      longPressTimer = setTimeout(() => {
        longPressTimer = null;
        if (touchId === null || touchScrolled) return;
        startTouchSelection(touchStartX, touchStartY);
      }, LONG_PRESS_MS);
    };

    const handleTouchMove = (e: TouchEvent) => {
      if (touchId === null) return;
      let touch: Touch | undefined;
      for (let i = 0; i < e.changedTouches.length; i++) {
        if (e.changedTouches[i]!.identifier === touchId) {
          touch = e.changedTouches[i]!;
          break;
        }
      }
      if (!touch) return;

      // While selecting, drag extends the selection toward the finger.
      if (touchSelecting && touchSelAnchor) {
        e.preventDefault();
        const cell = mouseToCell(
          new MouseEvent("touch", {
            clientX: touch.clientX,
            clientY: touch.clientY,
          }),
        );
        const sel = cellToSel(cell);
        if (selPosBefore(sel, touchSelAnchor)) {
          this.selStart = sel;
          this.selEnd = touchSelAnchor;
        } else {
          this.selStart = touchSelAnchor;
          this.selEnd = sel;
        }
        this.scheduleRender();
        this.notifySelectionChange();
        return;
      }

      // Cancel long-press if the finger drifts beyond the slop radius.
      if (longPressTimer !== null) {
        const dxAbs = Math.abs(touch.clientX - touchStartX);
        const dyAbs = Math.abs(touch.clientY - touchStartY);
        if (dxAbs > LONG_PRESS_SLOP_PX || dyAbs > LONG_PRESS_SLOP_PX) {
          cancelLongPress();
          touchScrolled = true;
        }
      }

      const dy = touchLastY - touch.clientY;
      touchLastY = touch.clientY;
      touchAccum += dy;

      const t = this.terminal;
      const lineH = this.cell.h || 20;

      // Emit one scroll event per cell-height of accumulated movement.
      while (Math.abs(touchAccum) >= lineH) {
        touchScrolled = true;
        const dir = touchAccum > 0 ? 1 : -1; // 1 = scroll up, -1 = scroll down
        touchAccum -= dir * lineH;

        if (t && t.mouse_mode() > 0) {
          // Mouse mode — send wheel button events (64=up, 65=down)
          const button = dir > 0 ? 64 : 65;
          const pos = mouseToCell(
            new MouseEvent("wheel", {
              clientX: touch.clientX,
              clientY: touch.clientY,
            }),
          );
          this._workspace?.sendMouse(
            this._sessionId!,
            MOUSE_DOWN,
            button,
            pos.col,
            pos.row,
          );
        } else if (this._sessionId !== null && this.status === "connected") {
          // Normal mode — scrollback
          const maxScroll = t ? t.scrollback_lines() : 0;
          if (maxScroll > 0 || this.scrollOffset > 0) {
            this.scrollOffset = Math.max(
              0,
              Math.min(maxScroll, this.scrollOffset + dir),
            );
            this.sendScroll(this._sessionId!, this.scrollOffset);
            if (this.scrollOffset > 0) this.flashScrollbar();
            this.scheduleRender();
          }
        }
        e.preventDefault();
      }
    };

    const handleTouchEnd = (e: TouchEvent) => {
      for (let i = 0; i < e.changedTouches.length; i++) {
        if (e.changedTouches[i]!.identifier === touchId) {
          cancelLongPress();
          touchId = null;
          touchAccum = 0;
          if (touchSelecting) {
            // Selection persists past touchend; the toolbar's Copy button
            // can act on it within the user's next gesture.
            touchSelecting = false;
            touchSelAnchor = null;
            // Suppress the synthetic mousedown/click iOS dispatches after
            // a long-press touch sequence, otherwise our mouse handler
            // would clear the freshly built selection.
            e.preventDefault();
          }
          break;
        }
      }
    };

    canvas.addEventListener("touchstart", handleTouchStart, { passive: true });
    canvas.addEventListener("touchmove", handleTouchMove, { passive: false });
    canvas.addEventListener("touchend", handleTouchEnd, { passive: false });
    canvas.addEventListener("touchcancel", handleTouchEnd, { passive: false });

    canvas.addEventListener("mousedown", handleMouseDown);
    window.addEventListener("mousemove", handleMouseMove);
    canvas.addEventListener("mousemove", handleHoverMove);
    window.addEventListener("mouseup", handleMouseUp);
    window.addEventListener("blur", handleBlur);
    canvas.addEventListener("wheel", handleCanvasWheel, { passive: false });
    canvas.addEventListener("contextmenu", handleContextMenu);
    canvas.addEventListener("click", handleClick);

    this.mouseCleanup = () => {
      canvas.removeEventListener("touchstart", handleTouchStart);
      canvas.removeEventListener("touchmove", handleTouchMove);
      canvas.removeEventListener("touchend", handleTouchEnd);
      canvas.removeEventListener("touchcancel", handleTouchEnd);
      canvas.removeEventListener("mousedown", handleMouseDown);
      window.removeEventListener("mousemove", handleMouseMove);
      canvas.removeEventListener("mousemove", handleHoverMove);
      window.removeEventListener("mouseup", handleMouseUp);
      window.removeEventListener("blur", handleBlur);
      canvas.removeEventListener("wheel", handleCanvasWheel);
      canvas.removeEventListener("contextmenu", handleContextMenu);
      canvas.removeEventListener("click", handleClick);
      if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
      stopAutoScroll();
    };
  }

  private teardownMouse(): void {
    this.mouseCleanup?.();
    this.mouseCleanup = null;
  }

  // --- Helpers ---

  private flashScrollbar(): void {
    this.scrollFade = 1;
    if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
    this.scrollFadeTimer = setTimeout(() => {
      this.scrollFade = 0;
      this.scheduleRender();
    }, 1000);
  }

  private sendInput(sessionId: SessionId, data: Uint8Array): void {
    this._workspace?.sendInput(sessionId, data);
  }

  private sendScroll(sessionId: SessionId, offset: number): void {
    this._workspace?.scrollSession(sessionId, offset);
  }
}
