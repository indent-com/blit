import type { Terminal } from "@blit-sh/browser";
import type { BlitWorkspace } from "./BlitWorkspace";
import type { BlitConnection } from "./BlitConnection";
import type { TerminalPalette, ConnectionStatus, SessionId } from "./types";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE, DEFAULT_TEXT_GAMMA } from "./types";
import { cancelFrame, scheduleFrame } from "./frameScheduler";
import { measureCell, cssFontFamily, type CellMetrics } from "./measure";
import type { GlRenderer } from "./gl-renderer";
import { keyToBytes, ctrlCharToByte, encoder } from "./keyboard";
import { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./protocol";
import { assessUrl, openUrlSafely, type UrlAssessment } from "./urlSecurity";
import { devicePixelBox, drawHalved, halve, halvings } from "./downscale";
import { WheelDetents } from "./wheel";

/** One screen row's slice of a hyperlink's extent, inclusive of both columns. */
interface LinkSegment {
  row: number;
  startCol: number;
  endCol: number;
}

/** A hyperlink under the pointer, from OSC 8 or from regex detection. */
interface UrlHit {
  url: string;
  /**
   * Every row the link covers. A link that runs past the right edge continues
   * on the next row, so this has more than one entry for a wrapped link and
   * the highlight is drawn across all of them.
   */
  segments: LinkSegment[];
  /** True when the application declared this link via OSC 8. */
  explicit: boolean;
}

/** What the pointer is currently over, handed to `onLinkHover` listeners. */
export interface LinkHover {
  assessment: UrlAssessment;
  /**
   * True for an OSC 8 link, where the application chose the target
   * independently of the text on screen. A regex-detected link is its own
   * text, so there is nothing for it to misrepresent; an explicit one is the
   * case where showing the user the real target actually matters.
   */
  explicit: boolean;
  /** The on-screen text of the link, for comparison against the target. */
  text: string;
}

// The ^V control byte.  Sent for a plain Ctrl+V (quoted-insert in shells, and
// the paste-trigger TUIs like Claude Code use to read the clipboard).
const CTRL_V = 0x16;

/** Screenshots land far below this; anything above it is not something a
 *  paste should risk the session on. */
const MAX_CLIPBOARD_BYTES = 8 * 1024 * 1024;

/** The mounted terminal surface owning each hidden keyboard textarea. */
const terminalSurfaceByInput = new WeakMap<
  HTMLTextAreaElement,
  BlitTerminalSurface
>();

/** Resolve a terminal surface from the hidden textarea that currently holds
 * keyboard focus. UI chrome uses this instead of retaining whichever split
 * happened to mount last. */
export function terminalSurfaceForInput(
  input: Element | null,
): BlitTerminalSurface | null {
  return input instanceof HTMLTextAreaElement
    ? (terminalSurfaceByInput.get(input) ?? null)
    : null;
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export interface BlitTerminalSurfaceOptions {
  sessionId: SessionId | null;
  fontFamily?: string;
  fontSize?: number;
  palette?: TerminalPalette;
  readOnly?: boolean;
  /** Resize the remote session to this surface. Disable for passive previews. Default: true. */
  resizable?: boolean;
  showCursor?: boolean;
  onRender?: (renderMs: number) => void;
  scrollbarColor?: string;
  scrollbarWidth?: number;
  advanceRatio?: number;
  /** Coverage gamma for glyph antialiasing. See DEFAULT_TEXT_GAMMA. */
  textGamma?: number;
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

/** Move a selection endpoint `lines` further from the live bottom, keeping it
 *  on the text it was on when the view underneath it is re-anchored.  Returns
 *  a new object: the same endpoint may be held elsewhere as a drag anchor. */
function shiftSelPos(pos: SelPos | null, lines: number): SelPos | null {
  if (!pos || lines === 0) return pos;
  return { ...pos, tailOffset: pos.tailOffset + lines };
}

// ---------------------------------------------------------------------------
// DPR detection
// ---------------------------------------------------------------------------

function isSafari(): boolean {
  if (typeof navigator === "undefined") return false;
  return /^((?!chrome|android).)*safari/i.test(navigator.userAgent);
}

function isIPadOS(): boolean {
  if (typeof navigator === "undefined") return false;
  // Modern iPadOS often reports itself as Macintosh; maxTouchPoints is the
  // reliable discriminator from desktop Safari.  The Safari outer/inner-width
  // zoom heuristic below is only valid on desktop; on iPad it double-counts
  // viewport scaling and can produce huge backing DPR/text rasters.
  return (
    /iPad/.test(navigator.platform) ||
    (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1)
  );
}

function isAndroid(): boolean {
  if (typeof navigator === "undefined") return false;
  return /android/i.test(navigator.userAgent);
}

function isIOS(): boolean {
  if (typeof navigator === "undefined") return false;
  // iPadOS reports as MacIntel — isIPadOS() covers that via maxTouchPoints.
  return isIPadOS() || /iPhone|iPod/.test(navigator.platform);
}

export { isIOS };

// iOS soft keyboards only auto-repeat Backspace while the focused field still
// has content to delete.  The hidden capture textarea is otherwise empty, so a
// held Backspace fires a single deleteContentBackward and stops.  We keep the
// textarea seeded with this filler run so iOS's own key-repeat streams a
// deleteContentBackward per repeat; each one forwards a DEL and consumes one
// filler char.  U+00A0 (NBSP) is a real, deletable character the user will
// never type, so it is trivial to strip back off the typed-text path.
const IOS_PAD_CODE = 0x00a0;
const IOS_PAD = String.fromCharCode(IOS_PAD_CODE).repeat(64);

/** Strip the leading NBSP filler run seeded into the iOS capture textarea,
 *  leaving only the text the user actually typed/pasted. */
function stripIosPad(value: string): string {
  let i = 0;
  while (i < value.length && value.charCodeAt(i) === IOS_PAD_CODE) i++;
  return value.slice(i);
}

/** The zoom levels Safari's View ▸ Zoom ladder can actually be set to. */
const SAFARI_ZOOM_STEPS = [
  0.5, 0.75, 0.85, 1, 1.15, 1.25, 1.5, 1.75, 2, 2.5, 3,
];

function effectiveDpr(): number {
  if (typeof window === "undefined") return 1;
  const base = window.devicePixelRatio || 1;
  if (isSafari() && !isIPadOS() && window.outerWidth && window.innerWidth) {
    // Desktop Safari does not fold page zoom into devicePixelRatio, so zoom has
    // to be inferred, and outerWidth/innerWidth is the only signal on offer.
    // But that ratio is not purely zoom: anything that eats viewport width
    // without shrinking the window — a sidebar, above all — inflates it
    // identically, and an over-estimated DPR means an oversized backing store
    // that the browser then resamples down, blurring every glyph. So only
    // believe a ratio that lands on a zoom level Safari can be set to, and
    // read the rest as "no zoom, just furniture".
    const ratio = window.outerWidth / window.innerWidth;
    const zoom = SAFARI_ZOOM_STEPS.find(
      (step) => Math.abs(ratio - step) <= step * 0.015,
    );
    if (zoom !== undefined) return Math.round(base * zoom * 100) / 100;
  }
  return base;
}

// ---------------------------------------------------------------------------
// Scroll surface stylesheet — WebKit/Blink expose no JS property to hide
// the scrollbar, so we ship a one-shot stylesheet on first attach.
// ---------------------------------------------------------------------------

/** Quiet time after the last scroll event that ends a gesture, for the
 *  purpose of leaving its scroll position alone. Long enough to bridge the
 *  gap between two frames of a momentum tail, short enough that a snap the
 *  user could notice never waits on it. */
const SCROLL_SETTLE_MS = 150;

let scrollSurfaceStylesInjected = false;
function injectScrollSurfaceStyles(): void {
  if (scrollSurfaceStylesInjected || typeof document === "undefined") return;
  scrollSurfaceStylesInjected = true;
  const style = document.createElement("style");
  style.setAttribute("data-blit-scroll-surface", "");
  style.textContent =
    ".blit-scroll-surface::-webkit-scrollbar{width:0;height:0;display:none}";
  document.head.appendChild(style);
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
  private _resizable: boolean;
  private _showCursor: boolean;
  private _onRender: ((renderMs: number) => void) | undefined;
  private _scrollbarColor: string | undefined;
  private _scrollbarWidth: number;
  private _advanceRatio: number | undefined;
  private _textGamma: number;

  // --- external collaborators ---
  private _workspace: BlitWorkspace | null = null;
  private _blitConn: BlitConnection | null = null;

  // --- DOM elements ---
  private container: HTMLDivElement | null = null;
  private glCanvas: HTMLCanvasElement | null = null;
  private inputEl: HTMLTextAreaElement | null = null;
  /** Transparent overlay sized to the canvas that captures pointer/wheel/
   *  touch input and provides native scrolling for scrollback navigation. */
  private scrollEl: HTMLDivElement | null = null;
  /** Inner spacer that gives `scrollEl` enough scrollable content height
   *  for the current scrollback range. */
  private scrollSpacer: HTMLDivElement | null = null;
  /** True while we're updating `scrollEl.scrollTop` from inside our own
   *  scrollOffset → scrollTop sync, so the scroll listener doesn't feed
   *  the change back. */
  private suppressScrollSync = false;
  /** scrollEl's client height, refreshed from the ResizeObserver and the
   *  scroll listener — both of which run after layout, so the measurement
   *  costs nothing. Never read inside the render loop (see
   *  syncScrollSurface). */
  private scrollViewH = 0;
  /** The last scrollTop we assigned, so the render loop can tell whether a
   *  write is needed without reading the element back. */
  private lastScrollTop = 0;
  /** When the user last moved the scroll surface themselves, so the render
   *  loop can keep its hands off a gesture that is still in flight. */
  private lastUserScrollAt = 0;
  /** Rows the server's re-anchor moved scrollOffset by since the last
   *  syncScrollSurface, so the sync can tell anchor-driven drift (deferrable
   *  mid-gesture) from an external jump (lands immediately). */
  private anchorRowsSinceSync = 0;

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
  /** Last composited device pixel size, used to detect resizes and schedule a
   *  one-frame catch-up render on the WebGPU backend (see doRender). */
  private lastRenderedPw = 0;
  private lastRenderedPh = 0;
  private lastWasmBuffer: ArrayBuffer | null = null;
  private raf = 0;
  private renderScheduled = false;
  /** Correction computed by `measureSnap`, awaiting `applySnap`. */
  private pendingSnap: [number, number] | null = null;
  /** Last known canvas box, reused by mouse handlers. Null means "re-read
   *  on next use". Mouse events fire many times per frame, and reading the
   *  box in each one forced a style recalc + layout every time — a profile
   *  of pointer movement put ~9% of the whole recording in Recalculate
   *  style, blamed on `mouseToCell`. */
  private canvasRect: DOMRect | null = null;
  /** Last value written to the scroll surface's cursor, so a mousemove that
   *  changes nothing does not dirty style. Writing the same value still
   *  invalidates it, which is what made the read above expensive.
   *  Re-seeded to the element's inline baseline in `attach()`. */
  private lastCursor = "";
  private dpr: number;
  /** Sub-pixel correction currently applied to the canvas, in CSS px.
   *  See snapToDevicePixels. */
  private snapX = 0;
  private snapY = 0;

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
  /** The word/line a granularity drag started on, held so the selection can
   *  grow outward from it in either direction. Fields rather than locals in
   *  the mouse handlers: like `selStart`/`selEnd` they are positions in the
   *  scrollback, so they travel when a scrolled view is re-anchored. */
  private selAnchorStart: SelPos | null = null;
  private selAnchorEnd: SelPos | null = null;
  /** Where a touch selection was anchored, for the same reason. */
  private touchSelAnchor: SelPos | null = null;
  private _selectionListeners = new Set<(hasSelection: boolean) => void>();
  private hoveredUrl: {
    segments: LinkSegment[];
    url: string;
    assessment: UrlAssessment;
  } | null = null;
  private _linkHoverListeners = new Set<(h: LinkHover | null) => void>();
  private _linkActivate: ((a: UrlAssessment) => void) | null = null;

  private predicted = "";
  private predictedFromRow = 0;
  private predictedFromCol = 0;

  private disposed = false;
  private _ctrlModifier = false;
  private _ctrlModifierListeners = new Set<(active: boolean) => void>();
  private _altModifier = false;
  private _altModifierListeners = new Set<(active: boolean) => void>();
  /** Tracks the composition string already forwarded to the shell on Android,
   *  so insertCompositionText updates can be streamed letter-by-letter instead
   *  of waiting for compositionend and dumping the whole word at once. */
  private _androidCompositionValue = "";
  /** True when the hidden textarea is kept seeded with filler so iOS soft
   *  keyboards auto-repeat a held Backspace (see IOS_PAD). */
  private _iosPad = false;
  /** Idle timer that tops the iOS filler buffer back up once a Backspace
   *  repeat burst ends (re-padding mid-burst would cancel iOS's repeat). */
  private _iosRepadTimer: ReturnType<typeof setTimeout> | null = null;

  // --- subscriptions / observers ---
  private dirtyUnsub: (() => void) | null = null;
  private scrollAnchorUnsub: (() => void) | null = null;
  private resizeObserver: ResizeObserver | null = null;
  /** Used by BlitConnection to reap a surface whose HMR cleanup was skipped. */
  private readonly viewIsActive = () => this.container?.isConnected === true;
  private dprMq: MediaQueryList | null = null;
  private dprCheckHandler: (() => void) | null = null;
  /** Re-snaps the canvas when layout moves it, not only when a frame renders
   *  (see setupDeviceSnapping). */
  private snapScrollHandler: (() => void) | null = null;
  private fontsHandler: (() => void) | null = null;

  // --- event handler refs (for cleanup) ---
  private boundKeyDown: ((e: KeyboardEvent) => void) | null = null;
  private boundCompositionStart: (() => void) | null = null;
  private boundCompositionEnd: ((e: CompositionEvent) => void) | null = null;
  private boundInput: ((e: Event) => void) | null = null;
  private boundPaste: ((e: ClipboardEvent) => void) | null = null;
  private boundScrollListener: (() => void) | null = null;

  // --- Ctrl+V image-paste deferral ---
  // Ctrl+V is the paste shortcut TUIs like Claude Code read an image from the
  // clipboard on.  A textarea can't hold an image, so we grab it from the
  // browser `paste` event and offer it to the server clipboard *before*
  // letting the app process ^V.  These fields coordinate the keydown (which
  // arms the deferral) with the paste handler / fallback timer (which sends
  // the ^V byte once the clipboard has been forwarded).
  private _ctrlVPastePending = false;
  private _ctrlVFallbackTimer: ReturnType<typeof setTimeout> | null = null;
  private mouseCleanup: (() => void) | null = null;

  constructor(options: BlitTerminalSurfaceOptions) {
    this._sessionId = options.sessionId;
    this._fontFamily = options.fontFamily ?? DEFAULT_FONT;
    this._fontSize = options.fontSize ?? DEFAULT_FONT_SIZE;
    this._palette = options.palette;
    this._readOnly = options.readOnly ?? false;
    this._resizable = options.resizable ?? true;
    this._showCursor = options.showCursor ?? true;
    this._onRender = options.onRender;
    this._scrollbarColor = options.scrollbarColor;
    this._scrollbarWidth = options.scrollbarWidth ?? 4;
    this._advanceRatio = options.advanceRatio;
    this._textGamma = options.textGamma ?? DEFAULT_TEXT_GAMMA;

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
    // Reflect the transport's send-readiness, not the snapshot-level
    // "authenticating→connected" promotion. Input/resize/mouse sends succeed
    // as soon as the transport is connected; gating on the snapshot would
    // block interaction whenever S2C_READY/server activity is delayed or lost,
    // even though the server is already accepting C2S_CREATE2 and input.
    return this._blitConn?.transport.status ?? "disconnected";
  }

  focus(): void {
    this.inputEl?.focus();
    // Re-seed the iOS Backspace-repeat filler in case the field was cleared.
    this.seedIosPad();
  }

  /** Fill the hidden textarea with the NBSP filler buffer and park the cursor
   *  at the end, so a held Backspace on the iOS soft keyboard keeps having
   *  content to delete and iOS auto-repeats the deletion.  No-op off iOS. */
  private seedIosPad(): void {
    if (!this._iosPad) return;
    const input = this.inputEl;
    if (!input) return;
    if (this._iosRepadTimer !== null) {
      clearTimeout(this._iosRepadTimer);
      this._iosRepadTimer = null;
    }
    input.value = IOS_PAD;
    const end = IOS_PAD.length;
    try {
      input.setSelectionRange(end, end);
    } catch {
      // Some browsers reject setSelectionRange on a detached/hidden field.
    }
  }

  /** Top the filler buffer back up once a Backspace repeat burst has gone
   *  idle.  Re-padding while the burst is live would reset the field and
   *  cancel iOS's key-repeat, so we wait for a gap between deletions. */
  private scheduleIosRepad(): void {
    if (!this._iosPad) return;
    if (this._iosRepadTimer !== null) clearTimeout(this._iosRepadTimer);
    this._iosRepadTimer = setTimeout(() => {
      this._iosRepadTimer = null;
      this.seedIosPad();
    }, 400);
  }

  /** Reset the capture textarea after an input event: re-seed the iOS filler
   *  buffer, or just empty the field on every other platform. */
  private resetCaptureField(): void {
    if (this._iosPad) this.seedIosPad();
    else if (this.inputEl) this.inputEl.value = "";
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

  /**
   * Subscribe to hyperlink hover. The listener receives a classified
   * assessment, or null when the pointer leaves a link.
   *
   * Render `assessment.display` — never `assessment.raw`. The raw target can
   * contain codepoints that reorder or conceal the text around them, which is
   * precisely what a preview exists to defeat.
   */
  onLinkHover(listener: (h: LinkHover | null) => void): () => void {
    this._linkHoverListeners.add(listener);
    return () => this._linkHoverListeners.delete(listener);
  }

  /**
   * Replace the built-in link activation policy with a custom one, typically
   * to swap the blocking `window.confirm` for an in-app dialog.
   *
   * The handler receives an already-classified assessment and is responsible
   * for honouring its verdict: a `deny` must not be opened, and a `confirm`
   * must not be opened without asking. Pass null to restore the default.
   */
  setLinkActivateHandler(handler: ((a: UrlAssessment) => void) | null): void {
    this._linkActivate = handler;
  }

  private emitLinkHover(h: LinkHover | null): void {
    for (const l of this._linkHoverListeners) l(h);
  }

  private activateLink(hit: UrlHit): void {
    const assessment = assessUrl(hit.url);
    if (this._linkActivate) {
      this._linkActivate(assessment);
      return;
    }
    openUrlSafely(hit.url);
  }

  /** Clear any active selection. */
  clearSelection(): void {
    // The drag anchors are cleared unconditionally: they outlive a single
    // handler now, so a stale one left behind by a detach or a session swap
    // would grow the next word-drag out of a position nothing points at.
    this.selAnchorStart = null;
    this.selAnchorEnd = null;
    this.touchSelAnchor = null;
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
        ({ text } = await this._blitConn.copyRange(
          this._sessionId,
          start.tailOffset,
          start.col,
          end.tailOffset,
          end.col,
        ));
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
   * the pasted text, or null when nothing is available. An image-only
   * clipboard (e.g. a fresh phone screenshot) is forwarded to the server
   * clipboard instead, followed by a ^V so the app reads it — the same
   * convention as the Ctrl+V paste-event path.
   */
  async pasteFromClipboard(): Promise<string | null> {
    if (this._readOnly) return null;
    if (this._sessionId === null || this.status !== "connected") return null;
    let text = "";
    try {
      text = await navigator.clipboard.readText();
    } catch {
      // readText rejects for image-only clipboards on some browsers; the
      // image attempt below is the fallback.
    }
    if (text) {
      this.pasteText(text);
      return text;
    }
    await this.pasteImageFromClipboard();
    return null;
  }

  /** Paste an image-only clipboard (e.g. a fresh phone screenshot) by
   *  pushing it to the server clipboard and triggering the app's read with
   *  ^V — the same convention as the Ctrl+V paste-event path.  Returns true
   *  when an image was forwarded. */
  private async pasteImageFromClipboard(): Promise<boolean> {
    if (typeof navigator.clipboard.read !== "function") return false;
    const conn = this._blitConn;
    const sid = this._sessionId;
    if (!conn || sid === null) return false;
    let items: ClipboardItem[];
    try {
      items = await navigator.clipboard.read();
    } catch {
      return false; // empty clipboard, or read() rejected
    }
    // Same preference order as BlitSurfaceCanvas: PNG is what every toolkit
    // asks for.
    for (const mime of ["image/png", "image/webp", "image/jpeg"]) {
      const item = items.find((i) => i.types.includes(mime));
      if (!item) continue;
      try {
        const buf = await (await item.getType(mime)).arrayBuffer();
        if (buf.byteLength > MAX_CLIPBOARD_BYTES) {
          console.warn(
            `blit: clipboard image is ${buf.byteLength} bytes, over the ` +
              `${MAX_CLIPBOARD_BYTES}-byte paste limit — not pasted`,
          );
          return false;
        }
        if (this._sessionId !== sid || this.status !== "connected")
          return false;
        // Transport messages are ordered, so the clipboard is populated
        // server-side before the ^V input arrives and the app reads it.
        conn.sendClipboard(mime, new Uint8Array(buf));
        this.sendInput(sid, new Uint8Array([CTRL_V]));
        return true;
      } catch {
        return false;
      }
    }
    return false;
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

  private applyCanvasLayout(): void {
    if (!this.glCanvas) return;

    // Both branches leave width/height to doRender, which sizes the element to
    // the grid's natural device pixels every frame.
    if (this._resizable) {
      Object.assign(this.glCanvas.style, {
        display: "block",
        maxWidth: "",
        maxHeight: "",
        margin: "",
        objectFit: "",
        objectPosition: "",
        position: "absolute",
        top: "0",
        left: "0",
        // Pointer/wheel/touch input is handled by `scrollEl` which sits
        // on top of the canvas — let those events fall through.
        pointerEvents: "none",
      });
    } else {
      Object.assign(this.glCanvas.style, {
        display: "block",
        // Clamp rather than stretch. `width: 100%` scaled the grid to whatever
        // box it was handed, which magnifies — and blurs — a preview whose
        // grid is smaller than its pane. With max-* the natural 1:1 size wins
        // whenever it fits, and only a grid too big for the box gets scaled
        // down (object-fit keeps that proportional).
        maxWidth: "100%",
        maxHeight: "100%",
        margin: "auto",
        objectFit: "contain",
        objectPosition: "center",
        // Stays in flow: preview cards size their own height off this canvas,
        // and an absolutely positioned one collapses them to nothing.
        position: "",
        top: "",
        left: "",
        pointerEvents: "",
      });
    }
    // The box moves, so any sub-pixel correction against the old one is stale.
    this.snapX = 0;
    this.snapY = 0;
    this.glCanvas.style.transform = "";
  }

  /** Attach to a container element. Creates the canvas + textarea inside it. */
  attach(container: HTMLDivElement): void {
    if (this.container === container) return;
    this.detach();
    this.container = container;

    // Create canvas
    this.glCanvas = document.createElement("canvas");
    this.applyCanvasLayout();
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
      // Fixed at the top of the screen, not of the pane: an assist target
      // iPadOS can always keep clear of the software keyboard, so no reveal
      // pan is ever needed on focus — whatever the pane's position in a
      // split.  When <main> is pinned and transformed (keyboard up), fixed
      // resolves against it, and its top is the screen's top either way.
      position: "fixed",
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
    terminalSurfaceByInput.set(this.inputEl, this);

    // Native scroll surface — sits over the canvas, captures all pointer/
    // wheel/touch input, and lets the browser handle scrollback navigation
    // with native momentum. For read-only views we don't render a scroll
    // surface (no scrollback navigation to expose).
    if (!this._readOnly) {
      this.scrollEl = document.createElement("div");
      Object.assign(this.scrollEl.style, {
        position: "absolute",
        inset: "0",
        // Vertical native scroll; horizontal is never scrollable.
        overflowX: "hidden",
        overflowY: "auto",
        // Allow vertical pan to scroll natively; custom JS handlers still
        // receive touchmove and can preventDefault for selection / mouse-
        // mode reporting.
        touchAction: "pan-y",
        // The terminal draws its own scrollbar; hide the browser one.
        scrollbarWidth: "none",
        // Same caret affordances as the canvas had.
        cursor: "text",
        userSelect: "none",
        WebkitUserSelect: "none",
        WebkitTouchCallout: "none",
        zIndex: "1",
        background: "transparent",
      });
      // setCursor dedups against this baseline, so the cache starts where the
      // element does.  attach() after a detach() builds a fresh element with
      // the inline "text" above; without this it would inherit the previous
      // one's idea of what it is showing and skip the next real change.
      this.lastCursor = "text";
      // WebKit/Blink scrollbar hider (no JS-readable property for it).
      this.scrollEl.classList.add("blit-scroll-surface");
      injectScrollSurfaceStyles();

      this.scrollSpacer = document.createElement("div");
      Object.assign(this.scrollSpacer.style, {
        width: "1px",
        height: "0px",
        pointerEvents: "none",
      });
      this.scrollEl.appendChild(this.scrollSpacer);
      container.appendChild(this.scrollEl);
    }

    this.setupDprDetection();
    this.setupDeviceSnapping();
    this.setupCursorBlink();
    this.setupRenderer();
    this.setupCellMeasure();
    this.setupTerminal();
    this.setupDirtyListener();
    this.setupResizeObserver();
    this.setupRenderLoop();
    this.setupKeyboard();
    this.setupScrollSurface();
    this.setupMouse();
    this.scheduleRender();
  }

  /** Detach from the current container. Removes all DOM elements and listeners. */
  detach(): void {
    this.teardownMouse();
    this.teardownScrollSurface();
    this.teardownKeyboard();
    this.teardownRenderLoop();
    this.teardownResizeObserver();
    this.teardownDirtyListener();
    this.teardownTerminal();
    this.teardownCellMeasure();
    this.teardownRenderer();
    this.teardownCursorBlink();
    this.teardownDeviceSnapping();
    this.teardownDprDetection();

    if (this.glCanvas && this.container?.contains(this.glCanvas)) {
      this.container.removeChild(this.glCanvas);
    }
    if (this.inputEl) {
      terminalSurfaceByInput.delete(this.inputEl);
      if (this.container?.contains(this.inputEl)) {
        this.container.removeChild(this.inputEl);
      }
    }
    if (this.scrollEl && this.container?.contains(this.scrollEl)) {
      this.container.removeChild(this.scrollEl);
    }
    this.glCanvas = null;
    this.inputEl = null;
    this.scrollEl = null;
    this.scrollSpacer = null;
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
  setReadOnly(readOnly: boolean | undefined): void {
    this._readOnly = readOnly ?? false;
  }

  setResizable(resizable: boolean | undefined): void {
    const resolved = resizable ?? true;
    if (this._resizable === resolved) return;
    this._resizable = resolved;
    this.applyCanvasLayout();
    if (!this.container) return;

    this.teardownResizeObserver();
    if (resolved) {
      this.remeasureCells(true);
    } else if (this.terminal) {
      this.syncTerminalSize(this.terminal);
    }
    // Both modes observe — a resizable pane to drive the grid, a passive one
    // to learn how far its canvas is about to be minified — and
    // setupResizeObserver picks the branch off _resizable.
    this.setupResizeObserver();
    this.contentDirty = true;
    this.scheduleRender();
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

  setTextGamma(gamma: number | undefined): void {
    const resolved = gamma ?? DEFAULT_TEXT_GAMMA;
    if (this._textGamma === resolved) return;
    this._textGamma = resolved;
    // Purely a shader term — no atlas or geometry change, just repaint.
    this.scheduleRender();
  }

  // =========================================================================
  // Private setup/teardown methods
  // =========================================================================

  private scheduleRender(): void {
    if (this.disposed) return;
    // One frame for every surface, staged reads-then-writes — see
    // ./frameScheduler. Per-surface rAFs meant pane N's layout read was
    // forced by pane N-1's writes.
    scheduleFrame(this);
  }

  /** Frame phase 1. Reads layout; writes nothing. */
  measureFrame(): void {
    if (this.disposed) return;
    this.measureSnap();
  }

  /** Frame phase 2. Writes and paints; reads no layout. */
  paintFrame(): void {
    if (this.disposed) return;
    this.applySnap();
    this.doRender();
    // doRender sizes the canvas, so whatever the measure phase cached is
    // stale now. Dropping it here means the next mouse event re-reads
    // once, rather than every event re-reading.
    this.invalidateCanvasBox();
  }

  // --- DPR detection ---

  private setupDprDetection(): void {
    this.dprCheckHandler = () => {
      const next = effectiveDpr();
      // Hysteresis. On Safari `effectiveDpr` infers zoom from
      // outerWidth/innerWidth, whose ratio drifts continuously while a
      // window is dragged (the chrome is a fixed pixel offset). An exact
      // inequality therefore fires most frames of a resize, and each one
      // costs a full remeasure: a throwaway canvas + measureText, then
      // invalidate_render_cache — the glyph atlas rebuilt, per pane, per
      // frame. Only a real zoom step clears this threshold.
      if (Math.abs(next - this.dpr) > 0.05) {
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

  /**
   * Keep the canvas on the device-pixel grid when layout moves it without a
   * frame to piggyback on.
   *
   * {@link snapToDevicePixels} runs inside `doRender`, which is enough while
   * frames arrive but not otherwise: a pane whose origin moves for a reason of
   * its own — chrome above it changing height, a dock opening — keeps its stale
   * correction until the server happens to send an update, and until then every
   * glyph is resampled off-grid.
   *
   * Deliberately no ResizeObserver of its own. A resizable surface already has
   * one (`setupResizeObserver`) and the snap hangs off that; a passive surface
   * must not register its container size at all, which a second observer would
   * do. Scroll covers movement that changes no box.
   */
  private setupDeviceSnapping(): void {
    this.snapScrollHandler = () => {
      // Scrolling an ancestor moves the box without resizing it.
      this.invalidateCanvasBox();
      this.snapToDevicePixels();
    };
    // Capture: an ancestor scrolling moves this canvas without any event of
    // its own reaching it.
    window.addEventListener("scroll", this.snapScrollHandler, true);
  }

  private teardownDeviceSnapping(): void {
    if (this.snapScrollHandler) {
      window.removeEventListener("scroll", this.snapScrollHandler, true);
      this.snapScrollHandler = null;
    }
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
    const t = this.terminal;
    if (t) {
      // Glyph metrics are local rendering state, not a remote resize request.
      // Passive previews still need them: after a reload they may be the first
      // surface for a terminal created with TerminalStore's 1x1 defaults.
      t.set_cell_size(cell.pw, cell.ph);
      t.set_font_family(this._fontFamily);
      t.set_font_size(rasterFontSize);
      if (shouldInvalidate) t.invalidate_render_cache();
    }
    if (this._resizable && this._blitConn) {
      this._blitConn.setCellSize(cell.pw, cell.ph);
      this._blitConn.setFontFamily(this._fontFamily);
      this._blitConn.setFontSize(rasterFontSize);
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

  // --- Terminal lifecycle ---

  private setupTerminal(): void {
    if (!this._blitConn) {
      this.terminal = null;
      return;
    }
    if (this._sessionId !== null) {
      this._blitConn.retain(this._sessionId);
      const t = this._blitConn.getTerminal(this._sessionId);
      if (t) {
        this.terminal = t;
        this.applyPaletteToTerminal(t);
        this.applyMetricsToTerminal(t);
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
  }

  // --- Dirty listener ---

  private setupDirtyListener(): void {
    if (!this._blitConn || this._sessionId === null) return;
    const conn = this._blitConn;
    const sessionId = this._sessionId;
    this.setupScrollAnchorListener();
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
      if (!this._resizable) this.syncTerminalSize(t);
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
      if (!this._resizable) this.syncTerminalSize(t);
    }
  }

  private teardownDirtyListener(): void {
    this.dirtyUnsub?.();
    this.dirtyUnsub = null;
    this.scrollAnchorUnsub?.();
    this.scrollAnchorUnsub = null;
  }

  /**
   * Follow the server's re-anchoring of a scrolled-back view.
   *
   * The offset names a distance from the live bottom, so it has to grow as
   * the app prints for the text to stay where the reader left it. The
   * server does that arithmetic — it is the one that knows how many lines
   * scrolled, including once the scrollback is full and the depth stops
   * growing — and we take its answer so the scrollbar, the selection
   * anchors, and the next offset we send all keep meaning the same rows the
   * frames do.
   */
  private setupScrollAnchorListener(): void {
    if (!this._blitConn || this._sessionId === null) return;
    this.scrollAnchorUnsub = this._blitConn.addScrollAnchorListener(
      this._sessionId,
      (offset) => {
        if (offset === this.scrollOffset) return;
        const moved = offset - this.scrollOffset;
        this.scrollOffset = offset;
        // A selection is anchored to the bottom the same way the view is,
        // so it has to travel with it — otherwise the highlight crawls off
        // the words it was on, which is exactly what someone copying out of
        // a scrollback while the app keeps printing would be doing. The
        // drag anchors go too: a re-anchor mid-drag would otherwise leave
        // the selection growing from where its first word used to be.
        this.selStart = shiftSelPos(this.selStart, moved);
        this.selEnd = shiftSelPos(this.selEnd, moved);
        this.selAnchorStart = shiftSelPos(this.selAnchorStart, moved);
        this.selAnchorEnd = shiftSelPos(this.selAnchorEnd, moved);
        this.touchSelAnchor = shiftSelPos(this.touchSelAnchor, moved);
        this.anchorRowsSinceSync += moved;
        // No scrollTop write here: while the scrollback is still growing,
        // the frame that goes with this offset deepens it by the same number
        // of lines, so the position the render loop computes doesn't move.
        // Once the scrollback is capped it *does* move, and syncScrollSurface
        // decides from anchorRowsSinceSync whether writing that compensation
        // is safe (parked view) or would stomp an in-flight gesture.
        this.scheduleRender();
      },
    );
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

  private syncTerminalSize(t: Terminal): void {
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
    if (!this.container) return;

    if (!this._resizable) {
      // A passive view must not register a container size with the server:
      // the grid is the minimum across a session's views, so a thumbnail
      // would pin the live pane to a card.  It still needs the box for
      // presentation — doRender composites the shared canvas down to roughly
      // this size, leaving CSS a scale it can actually filter — so observe,
      // but stop short of handleResize.
      this.resizeObserver = new ResizeObserver((entries) => {
        const entry = entries[entries.length - 1];
        const box = entry && devicePixelBox(entry);
        if (!box) return;
        if (
          this._presentBox?.width === box.width &&
          this._presentBox.height === box.height
        ) {
          return;
        }
        this._presentBox = box;
        this.scheduleRender();
      });
      this.resizeObserver.observe(this.container);
      return;
    }

    if (!this.viewId && this._blitConn) {
      this.viewId = this._blitConn.allocViewId();
    }

    // The ResizeObserver already fires for a window resize — the pane's
    // box changes with it — so a second `window` listener only doubled
    // the work per frame, per pane.
    this.resizeObserver = new ResizeObserver(() => {
      // Refresh the cached scroll geometry here rather than in the render
      // loop: an observer callback runs after layout, so these reads are
      // already-computed values instead of a forced reflow.
      if (this.scrollEl) {
        this.scrollViewH = this.scrollEl.clientHeight;
        this.lastScrollTop = this.scrollEl.scrollTop;
      }
      this.invalidateCanvasBox();
      // Snap before the size round-trip: the new box is already on screen, and
      // waiting for the server's grid would leave it off-grid until then.
      this.snapToDevicePixels();
      this.handleResize();
    });
    this.resizeObserver.observe(this.container);
    this.handleResize(true /* immediate */);
  }

  private teardownResizeObserver(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    clearTimeout(this._resizeTimer);
    if (this._sessionId !== null && this._blitConn && this.viewId) {
      this._blitConn.removeView(this._sessionId, this.viewId);
    }
  }

  private _resizeTimer: ReturnType<typeof setTimeout> | undefined;
  private _lastViewSizeAt = 0;
  /** Container CSS size, cached in handleResize (post-layout) so doRender
   *  can center a grid smaller than its pane without a forced reflow. */
  private _containerW = 0;
  private _containerH = 0;
  /** Container size in device pixels, tracked only for a non-resizable view.
   *  Presentation only — it never reaches handleResize, so a thumbnail can't
   *  drag the session's grid down to its own box. */
  private _presentBox: { width: number; height: number } | null = null;
  /** Wire rate limit for size changes. Low enough that a drag stays
   *  roughly live, high enough not to flood the server with intermediate
   *  sizes (each one can cost an encoder rebuild for h264-software). */
  private static readonly RESIZE_THROTTLE_MS = 80;

  private handleResize(immediate?: boolean): void {
    if (!this.container || !this._resizable) return;
    const w = this.container.clientWidth;
    const h = this.container.clientHeight;
    // Cached for doRender's centering math: an observer callback runs
    // after layout, so this read is free here and would force a reflow
    // in the render loop.
    this._containerW = w;
    this._containerH = h;
    const cols = Math.max(1, Math.floor(w / this.cell.w));
    const rows = Math.max(1, Math.floor(h / this.cell.h));
    const sizeChanged = cols !== this._cols || rows !== this._rows;
    // `immediate` is used when an observer is first installed. Register the
    // view even when its box happens to be exactly the 80x24 defaults; after an
    // HMR registry reset (and on an ordinary first mount) equality must not
    // turn the registration into a no-op.
    if (sizeChanged || immediate) {
      this._rows = rows;
      this._cols = cols;
      // The grid is server-owned: the wasm terminal's cols/rows are
      // read-only, so nothing on screen reflows until setViewSize has
      // round-tripped and the server streams the new grid back. A purely
      // trailing debounce therefore does not "delay the network message
      // while rendering locally" — it freezes the pane's contents for the
      // whole drag, because every frame's clearTimeout restarts it.
      //
      // Leading edge + throttle instead: the first change of a drag goes
      // out at once and further ones at most every RESIZE_THROTTLE_MS,
      // with a trailing timer so the final size always lands.
      if (this._sessionId !== null && this._blitConn && this.viewId) {
        const send = () => {
          if (this._sessionId === null || !this._blitConn || !this.viewId)
            return;
          this._lastViewSizeAt = performance.now();
          this._blitConn.setViewSize(
            this._sessionId,
            this.viewId,
            this._rows,
            this._cols,
            this.viewIsActive,
          );
        };
        clearTimeout(this._resizeTimer);
        const since = performance.now() - this._lastViewSizeAt;
        if (immediate || since >= BlitTerminalSurface.RESIZE_THROTTLE_MS) {
          send();
        } else {
          // Read _rows/_cols at fire time, not now: a later frame in the
          // same window updates them and this timer should carry the
          // newest size, not the one that scheduled it.
          this._resizeTimer = setTimeout(
            send,
            BlitTerminalSurface.RESIZE_THROTTLE_MS - since,
          );
        }
      }
    }
    // Changing the pane's height changes `clientHeight`, and the browser
    // re-clamps `scrollTop` to the new range — a synthetic scroll event the
    // listener would otherwise read as the user scrolling, derive a new
    // offset from, and send to the server. Opening a pane above a terminal
    // must not move its scrollback.
    //
    // Only the flag is set here. The render loop already calls
    // `syncScrollSurface` every frame, so doing it synchronously as well
    // added a forced layout read *and* two style writes per pane per
    // resize event — the interleaving that makes a window drag crawl.
    // Two frames of suppression because the clamp lands after layout,
    // by which point the scheduled render has re-derived the DOM from
    // `scrollOffset`, which is the value we actually trust.
    this.suppressScrollSync = true;
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        this.suppressScrollSync = false;
      }),
    );

    this.contentDirty = true;
    this.scheduleRender();
  }

  /** Re-send dimensions when connection becomes ready. */
  resendSize(): void {
    if (
      this._sessionId !== null &&
      this._resizable &&
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
        this.viewIsActive,
      );
    }
  }

  // --- Render loop ---

  private setupRenderLoop(): void {
    this.scheduleRender();
  }

  private teardownRenderLoop(): void {
    cancelAnimationFrame(this.raf);
    cancelFrame(this);
    this.renderScheduled = false;
  }

  /**
   * Cancel the canvas's fractional device-pixel offset.
   *
   * Everything about the backing store is device-pixel exact — cell metrics
   * snap to whole device pixels (measureCell), glyph quads land on integer
   * boundaries, the composite blit is 1:1. None of that survives if the
   * element itself is painted at a fractional offset, which is the normal
   * outcome of laying panes out with flex weights: the compositor resamples
   * the whole canvas and every glyph in it softens at once.
   *
   * So measure where the box actually lands and translate by the remainder.
   * The correction is always under one device pixel, and it is applied via
   * `transform` precisely because transforms do not perturb layout — nothing
   * reflows, and the sibling scroll/input overlays stay put.
   */
  /**
   * Read half of device-pixel snapping: work out the correction, write
   * nothing. Safe only while layout is clean — the frame scheduler's
   * measure phase, a ResizeObserver callback, or a scroll callback.
   */
  private measureSnap(): void {
    const canvas = this.glCanvas;
    if (!canvas) {
      this.pendingSnap = null;
      return;
    }
    const dpr = this.dpr;
    // At dpr 1 a correction is always ±0.5 CSS px, and a fractional transform
    // is worse than the problem: it promotes the canvas to its own composited
    // layer and Chrome rasterizes the bitmap *through* the translate,
    // resampling every pixel. Left alone, paint-time snapping puts a
    // fractionally-positioned canvas on the grid crisply. Above dpr 1 the
    // correction is a sub-CSS-pixel nudge that lands on a real device pixel,
    // which is worth the layer.
    //
    // Decided before the rect read, not after: the answer at dpr <= 1 is
    // always "no correction", so reading the box first would be a layout
    // read that could only confirm what we already knew.
    if (dpr <= 1) {
      this.pendingSnap = [0, 0];
      return;
    }
    const rect = canvas.getBoundingClientRect();
    // The measure phase is the one place a read is already paid for, so
    // the mouse handlers' cache is refreshed from it.
    this.canvasRect = rect;
    if (rect.width === 0 || rect.height === 0) {
      this.pendingSnap = null;
      return;
    }
    // getBoundingClientRect reports the *transformed* box, so back out the
    // correction already in place or it would compound frame over frame.
    const left = (rect.left - this.snapX) * dpr;
    const top = (rect.top - this.snapY) * dpr;
    this.pendingSnap = [
      (Math.round(left) - left) / dpr,
      (Math.round(top) - top) / dpr,
    ];
  }

  /** The canvas box, re-read only when something may have moved it. */
  private canvasBox(): DOMRect | null {
    const canvas = this.glCanvas;
    if (!canvas) return null;
    if (!this.canvasRect) this.canvasRect = canvas.getBoundingClientRect();
    return this.canvasRect;
  }

  /** Forget the cached box. Call whenever layout may have shifted it. */
  private invalidateCanvasBox(): void {
    this.canvasRect = null;
  }

  /**
   * Set the scroll surface's cursor, skipping a redundant write.
   *
   * The dedup is against {@link lastCursor}, which mirrors the element's
   * inline style — so the two are seeded together where `scrollEl` is
   * created.  Let them drift and the guard starts suppressing writes the
   * element never received.
   */
  private setCursor(target: HTMLElement, value: string): void {
    if (this.lastCursor === value) return;
    this.lastCursor = value;
    target.style.cursor = value;
  }

  /** Write half: apply whatever `measureSnap` worked out. */
  private applySnap(): void {
    const canvas = this.glCanvas;
    const snap = this.pendingSnap;
    this.pendingSnap = null;
    if (!canvas || !snap) return;
    const [dx, dy] = snap;
    if (dx === this.snapX && dy === this.snapY) return;
    this.snapX = dx;
    this.snapY = dy;
    canvas.style.transform =
      dx === 0 && dy === 0 ? "" : `translate(${dx}px, ${dy}px)`;
  }

  /**
   * Both halves back to back. Only for callers that already run after
   * layout — a ResizeObserver or scroll callback — where the read is a
   * cached value rather than a forced reflow. The render path must use the
   * split halves instead, or it reintroduces exactly the cross-pane
   * read-after-write this was built to remove.
   */
  private snapToDevicePixels(): void {
    this.measureSnap();
    this.applySnap();
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

    // Size the element to the grid's natural device pixels. In resizable
    // panes that is also the backing store, so the blit is 1:1 and that is the
    // whole story; non-resizable ones additionally clamp with
    // max-width/max-height (see applyCanvasLayout) so an oversized grid still
    // scales down to fit, but one that already fits is left at 1:1 instead of
    // being magnified. When the clamp bites, the composite below has already
    // halved the backing store towards the box, so the residual CSS scale is
    // always under 2:1.
    const cssW = `${termCols * cell.w}px`;
    // Non-resizable surfaces leave the height to the canvas's intrinsic aspect
    // ratio, so that clamping the width scales the grid instead of squashing
    // it and letterboxing the difference.
    const cssH = this._resizable ? `${termRows * cell.h}px` : "auto";
    const glCanvas = this.glCanvas;
    if (glCanvas) {
      if (glCanvas.style.width !== cssW) glCanvas.style.width = cssW;
      if (glCanvas.style.height !== cssH) glCanvas.style.height = cssH;
      if (this._resizable) {
        // The grid is server-owned (the minimum across subscribed clients),
        // so it can be smaller than this pane; center it rather than pin it
        // to the top-left corner.  Offsets are floored to whole CSS pixels —
        // measureSnap/applySnap absorb any fractional device-pixel residue
        // at dpr > 1 — and are 0 whenever the grid fills the pane.
        const cssLeft = `${Math.max(0, Math.floor((this._containerW - termCols * cell.w) / 2))}px`;
        const cssTop = `${Math.max(0, Math.floor((this._containerH - termRows * cell.h) / 2))}px`;
        if (glCanvas.style.left !== cssLeft || glCanvas.style.top !== cssTop) {
          glCanvas.style.left = cssLeft;
          glCanvas.style.top = cssTop;
          // The box moved: the sub-pixel snap computed against the old
          // position is stale.  Schedule another frame so measureSnap sees
          // the new box; it converges because these writes are guarded.
          this.scheduleRender();
        }
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

    let preparedOps = false;
    if (this.contentDirty) {
      this.contentDirty = false;
      preparedOps = true;
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
    // The renderer is shared between panes, so this is per-frame state, not
    // setup — a pane must not inherit its neighbour's gamma.
    renderer.setTextGamma(this._textGamma);
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

    // Copy GL to display canvas, then draw overlay content on top. This runs
    // synchronously right after render(), so each surface composites its own
    // just-rendered frame from the shared canvas — no cross-pane bleed.
    const shared = conn.getSharedRenderer();
    const displayCanvas = this.glCanvas;
    if (shared && displayCanvas) {
      // A grid too big for its box — a dock thumbnail, a preview card — is
      // minified by the browser, which takes a single bilinear tap and drops
      // most of every glyph.  Composite it down in whole halves first so what
      // is left for CSS to scale is under 2:1.  A resizable pane is already
      // 1:1, so n is 0 and this is the plain copy it always was.
      const box = this._resizable ? null : this._presentBox;
      const n = box ? halvings(pw, ph, box.width, box.height) : 0;
      const dw = halve(pw, n);
      const dh = halve(ph, n);
      if (displayCanvas.width !== dw) {
        displayCanvas.width = dw;
        this.displayCtx = null;
      }
      if (displayCanvas.height !== dh) {
        displayCanvas.height = dh;
        this.displayCtx = null;
      }
      if (!this.displayCtx) {
        this.displayCtx = displayCanvas.getContext("2d");
        this.displayCtx?.resetTransform();
      }
      const ctx = this.displayCtx;
      if (ctx) {
        drawHalved(ctx, shared.canvas, pw, ph, n);
        // The overlays lay themselves out in full-resolution grid pixels, so
        // scale the context to match rather than teaching each one about the
        // reduction.  setTransform is absolute: nothing accumulates.
        if (n) ctx.setTransform(dw / pw, 0, 0, dh / ph, 0, 0);
        this.drawSelectionOverlay(ctx, cell);
        this.drawUrlOverlay(ctx, cell);
        this.drawOverflowText(ctx, t, cell);
        this.drawPredictedEcho(ctx, t, cell);
        this.drawScrollbar(ctx, t, cell);
        if (n) ctx.resetTransform();
      }
    }

    // WebGPU presents asynchronously, so `drawImage(webgpuCanvas)` above reads
    // the *previously* presented frame, not the one just submitted. Rendering
    // is event-driven, so "the next render heals it" is only true if there is
    // a next render — and the last frame of a burst of output is exactly the
    // one with nothing behind it. Until the cursor blink fires (530ms, and
    // never on a read-only surface) the whole pane sits a frame behind.
    //
    // So catch up after any frame that changed what is on screen: new content
    // (`preparedOps`) or a new size. A resize is the worse of the two — the
    // stale frame is the wrong size, composited as whole-screen trails — but
    // both leave the pane stale for as long as the surface stays idle.
    //
    // This cannot loop: the catch-up render prepares nothing and changes no
    // size, so it schedules no third frame. Nor does it double the work while
    // output streams — `scheduleFrame` coalesces into a Set, so the catch-up
    // merges with the next content render if one lands in the same frame. The
    // extra frame is only ever paid on the way to idle.
    //
    // WebGL2 needs none of this: preserveDrawingBuffer gives a synchronous
    // same-frame readback.
    if (
      shared?.renderer.backend === "webgpu" &&
      (preparedOps || pw !== this.lastRenderedPw || ph !== this.lastRenderedPh)
    ) {
      this.scheduleRender();
    }
    this.lastRenderedPw = pw;
    this.lastRenderedPh = ph;

    // Keep the native scroll surface in sync with the current scrollback
    // depth and offset.  Cheap idempotent — only touches the DOM when
    // values actually changed.
    this.syncScrollSurface(/* preserveOffset */ true);

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
    const verdict = hurl.assessment.verdict;
    const [fgR, fgG, fgB] = this._palette?.fg ?? [204, 204, 204];
    ctx.lineWidth = Math.max(1, Math.round(cell.ph * 0.06));
    // A blocked link is dashed and red rather than underlined, so a target
    // Blit will refuse never looks like one it is offering to open.
    if (verdict === "deny") {
      ctx.strokeStyle = "rgba(220,80,80,0.8)";
      ctx.setLineDash([cell.pw * 0.3, cell.pw * 0.3]);
    } else {
      ctx.strokeStyle = `rgba(${fgR},${fgG},${fgB},0.6)`;
      ctx.setLineDash([]);
    }
    ctx.beginPath();
    // One underline per row the link occupies — a wrapped link is one link,
    // and highlighting only the hovered row would misreport its extent.
    for (const seg of hurl.segments) {
      const y = seg.row * cell.ph + cell.ph - ctx.lineWidth;
      ctx.moveTo(seg.startCol * cell.pw, y);
      ctx.lineTo((seg.endCol + 1) * cell.pw, y);
    }
    ctx.stroke();
    ctx.setLineDash([]);
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

    // iOS soft keyboards need the capture textarea to stay non-empty for a
    // held Backspace to auto-repeat.  Read-only surfaces never take input.
    this._iosPad = !this._readOnly && isIOS();

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
          const prev = this.scrollOffset;
          this.scrollOffset = Math.max(
            0,
            Math.min(maxScroll, this.scrollOffset + delta),
          );
          this.sendScrollBy(
            this._sessionId!,
            this.scrollOffset,
            this.scrollOffset - prev,
          );
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

      // Ctrl+Shift+V pastes from the browser clipboard.  Ctrl+V is left as
      // the terminal's default ^V (quoted-insert) control character.
      if (
        e.ctrlKey &&
        e.shiftKey &&
        !e.altKey &&
        !e.metaKey &&
        (e.key === "v" || e.key === "V") &&
        !e.repeat
      ) {
        e.preventDefault();
        void this.pasteFromClipboard();
        return;
      }

      // Ctrl+V (no Shift): TUIs like Claude Code read an image from the
      // clipboard when they receive ^V.  A textarea can't surface a pasted
      // image via the `input` event, so we must let the browser fire a
      // `paste` event (do NOT preventDefault here), grab any image there, and
      // offer it to the server clipboard before ^V reaches the app.  The
      // paste handler / fallback timer sends the ^V byte itself.
      if (
        e.ctrlKey &&
        !e.shiftKey &&
        !e.altKey &&
        !e.metaKey &&
        (e.key === "v" || e.key === "V") &&
        !e.repeat
      ) {
        this.beginCtrlVPaste();
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

    this.boundCompositionStart = () => {
      this._androidCompositionValue = "";
    };

    this.boundCompositionEnd = (e: CompositionEvent) => {
      if (isAndroid()) {
        // On Android we stream insertCompositionText updates letter-by-letter
        // while the composition is active, so the final word has already been
        // sent.  Clear the capture buffer so the post-composition input event
        // (e.g. a space) doesn't duplicate the word.
        this._androidCompositionValue = "";
        input.value = "";
        return;
      }
      if (e.data && this._sessionId !== null && this.status === "connected") {
        this.sendInput(this._sessionId, encoder.encode(e.data));
      }
      // Re-seed the iOS filler so Backspace-repeat keeps working after a
      // dictation/accent composition (no-op off iOS → empties the field).
      this.resetCaptureField();
    };

    this.boundInput = (e: Event) => {
      const inputEvent = e as InputEvent;
      if (inputEvent.isComposing) {
        if (isAndroid()) {
          this.handleAndroidCompositionInput(inputEvent);
          return;
        }
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
      // iOS soft-keyboard Backspace: the textarea is kept seeded with NBSP
      // filler (see IOS_PAD) so a held Backspace always has content to delete
      // and iOS streams a deleteContentBackward per key-repeat.  Forward one
      // DEL each and leave the now-shorter buffer alone — re-padding here would
      // reset the field and cancel iOS's repeat.  Top it back up once the burst
      // goes idle, or immediately if it is about to run dry mid-hold.
      if (this._iosPad && inputEvent.inputType === "deleteContentBackward") {
        if (this._sessionId !== null && this.status === "connected") {
          this.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
        if (input.value.length <= 4) this.seedIosPad();
        else this.scheduleIosRepad();
        return;
      }
      // iPadOS (and desktop spellcheck) ignore autocorrect="off" on this
      // hidden capture textarea and instead deliver autocorrect/suggestion
      // substitutions as an "insertReplacementText" input event.  Each
      // literally-typed character has already been streamed to the shell as
      // its own insertText event, so forwarding the replacement would both
      // duplicate and "correct" terminal input.  Drop it — this is what makes
      // autocorrect-off actually stick on iPad keyboards.
      if (inputEvent.inputType === "insertReplacementText") {
        this.resetCaptureField();
        return;
      }
      // On iOS the field carries the filler buffer; strip it so we only act on
      // what the user actually typed/pasted.
      const typed = this._iosPad ? stripIosPad(input.value) : input.value;
      // Ctrl modifier: convert the next typed character to Ctrl+char
      if (
        this._ctrlModifier &&
        typed &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const char = typed[0];
        const bytes = ctrlCharToByte(char);
        if (bytes) {
          this.sendInput(this._sessionId, bytes);
        }
        this.setCtrlModifier(false);
        this.resetCaptureField();
        return;
      }
      // Alt modifier: prefix next typed character with ESC
      if (
        this._altModifier &&
        typed &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const char = typed[0];
        const charCode = char.charCodeAt(0);
        this.sendInput(this._sessionId, new Uint8Array([0x1b, charCode]));
        this.setAltModifier(false);
        this.resetCaptureField();
        return;
      }
      if (inputEvent.inputType === "deleteContentBackward" && !typed) {
        if (this._sessionId !== null && this.status === "connected") {
          this.sendInput(this._sessionId, new Uint8Array([0x7f]));
        }
      } else if (
        typed &&
        this._sessionId !== null &&
        this.status === "connected"
      ) {
        const payload = encoder.encode(typed.replace(/\n/g, "\r"));
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
      this.resetCaptureField();
    };

    this.boundPaste = (e: ClipboardEvent) => this.handlePaste(e);

    input.addEventListener("keydown", this.boundKeyDown);
    input.addEventListener("compositionstart", this.boundCompositionStart);
    input.addEventListener("compositionend", this.boundCompositionEnd);
    input.addEventListener("input", this.boundInput);
    input.addEventListener("paste", this.boundPaste);

    this.seedIosPad();
  }

  private teardownKeyboard(): void {
    const input = this.inputEl;
    if (!input) return;
    if (this.boundKeyDown)
      input.removeEventListener("keydown", this.boundKeyDown);
    if (this.boundCompositionStart)
      input.removeEventListener("compositionstart", this.boundCompositionStart);
    if (this.boundCompositionEnd)
      input.removeEventListener("compositionend", this.boundCompositionEnd);
    if (this.boundInput) input.removeEventListener("input", this.boundInput);
    if (this.boundPaste) input.removeEventListener("paste", this.boundPaste);
    if (this._ctrlVFallbackTimer !== null) {
      clearTimeout(this._ctrlVFallbackTimer);
      this._ctrlVFallbackTimer = null;
    }
    this._ctrlVPastePending = false;
    if (this._iosRepadTimer !== null) {
      clearTimeout(this._iosRepadTimer);
      this._iosRepadTimer = null;
    }
    this.boundKeyDown = null;
    this.boundCompositionStart = null;
    this.boundCompositionEnd = null;
    this.boundInput = null;
    this.boundPaste = null;
  }

  // --- Ctrl+V image paste ---------------------------------------------------

  /** Arm the Ctrl+V deferral: don't send ^V yet, wait for the `paste` event
   *  to forward any clipboard image first.  A fallback timer sends the raw
   *  ^V if no paste event materialises (empty clipboard, denied permission,
   *  or a browser that won't fire paste without content) so quoted-insert and
   *  app paste-triggers still work. */
  private beginCtrlVPaste(): void {
    if (this._sessionId === null || this.status !== "connected") return;
    // A pending press being replaced (autorepeat is filtered by !e.repeat, but
    // guard anyway): flush the old one as a plain ^V before re-arming.
    if (this._ctrlVFallbackTimer !== null) {
      clearTimeout(this._ctrlVFallbackTimer);
      this._ctrlVFallbackTimer = null;
    }
    // Scrolling back and pasting should jump to the live prompt, matching the
    // keyToBytes input path.
    if (this.scrollOffset > 0) {
      this.scrollOffset = 0;
      this.sendScroll(this._sessionId, 0);
    }
    this._ctrlVPastePending = true;
    this._ctrlVFallbackTimer = setTimeout(() => {
      this._ctrlVFallbackTimer = null;
      if (this._ctrlVPastePending) {
        this._ctrlVPastePending = false;
        this.sendCtrlV();
      }
    }, 0);
  }

  private sendCtrlV(): void {
    if (this._readOnly) return;
    if (this._sessionId === null || this.status !== "connected") return;
    this.sendInput(this._sessionId, new Uint8Array([CTRL_V]));
  }

  /** Find the first image entry on a clipboard payload, if any. */
  private findClipboardImage(dt: DataTransfer | null): DataTransferItem | null {
    const items = dt?.items;
    if (!items) return null;
    for (let i = 0; i < items.length; i++) {
      const it = items[i];
      if (it.kind === "file" && it.type.startsWith("image/")) return it;
    }
    return null;
  }

  private handlePaste(e: ClipboardEvent): void {
    if (this._readOnly) return;
    if (this._sessionId === null || this.status !== "connected") return;

    // Consume the pending Ctrl+V arm (if this paste came from Ctrl+V) so the
    // fallback timer doesn't also fire a ^V.
    const wasCtrlV = this._ctrlVPastePending;
    this._ctrlVPastePending = false;
    if (this._ctrlVFallbackTimer !== null) {
      clearTimeout(this._ctrlVFallbackTimer);
      this._ctrlVFallbackTimer = null;
    }

    const imageItem = wasCtrlV
      ? this.findClipboardImage(e.clipboardData)
      : null;

    if (imageItem) {
      // We own this paste: stop the textarea from doing anything with it (it
      // can't hold an image anyway) and forward the bytes to the server
      // clipboard, then trigger the app's read with ^V.
      e.preventDefault();
      const file = imageItem.getAsFile();
      const conn = this._blitConn;
      const sid = this._sessionId;
      if (!file || !conn) {
        this.sendCtrlV();
        return;
      }
      const mime = file.type || "image/png";
      void file
        .arrayBuffer()
        .then((buf) => {
          if (this._sessionId !== sid || this.status !== "connected") return;
          // Transport messages are ordered, so the clipboard is populated
          // server-side before the ^V input arrives and the app reads it.
          conn.sendClipboard(mime, new Uint8Array(buf));
          this.sendInput(sid, new Uint8Array([CTRL_V]));
        })
        .catch(() => {
          // Reading the blob failed — fall back to a bare ^V so the keypress
          // isn't swallowed entirely.
          this.sendCtrlV();
        });
      return;
    }

    if (wasCtrlV) {
      // Plain Ctrl+V with no image: preserve ^V (quoted-insert / paste-trigger)
      // and suppress the textarea's own text paste so we don't double-send.
      e.preventDefault();
      this.sendCtrlV();
      return;
    }

    // Cmd+V / context-menu text paste: clipboardData is available
    // synchronously on the paste event, so send it now instead of waiting for
    // the hidden textarea's later input(insertFromPaste) event.  WebKit can
    // defer that input until the next edit of an invisible textarea, which
    // makes the paste appear only after the user types another character.
    // Leave an empty/unavailable payload to the input-event fallback: some
    // browsers expose the text there but redact it from clipboardData.
    const text = e.clipboardData?.getData("text/plain") ?? "";
    if (text) {
      e.preventDefault();
      this.pasteText(text);
      this.resetCaptureField();
    }
  }

  /** Stream Android IME composition updates to the shell one character at a
   *  time.  Android soft keyboards (Gboard, Samsung) keep the whole word in
   *  an active composition and only commit it on space/suggestion, which
   *  makes the terminal feel like it accepts input word-by-word.  By sending
   *  the delta between consecutive composition values we get letter-by-letter
   *  behaviour for Latin input while still letting compositionend deliver the
   *  final result for non-Latin IMEs. */
  private handleAndroidCompositionInput(inputEvent: InputEvent): void {
    const input = this.inputEl;
    if (!input || this._sessionId === null || this.status !== "connected")
      return;

    const value = input.value;
    const oldValue = this._androidCompositionValue;

    if (inputEvent.inputType === "deleteContentBackward" && !value) {
      this.sendInput(this._sessionId, new Uint8Array([0x7f]));
      this._androidCompositionValue = value;
      return;
    }

    if (
      inputEvent.inputType !== "insertCompositionText" &&
      inputEvent.inputType !== "insertText"
    ) {
      return;
    }

    // The toolbar's one-shot Ctrl/Alt, on this path too. Android keyboards
    // keep the word in an active composition, so a letter typed after
    // tapping Ctrl arrives here and never reaches the keydown or plain
    // `input` branches that apply the modifier — Ctrl+C came out as a
    // literal "c". The composition is abandoned rather than continued: the
    // control byte is not text the IME can go on editing.
    if (
      (this._ctrlModifier || this._altModifier) &&
      value.startsWith(oldValue) &&
      value.length > oldValue.length
    ) {
      const char = value.slice(oldValue.length)[0];
      const bytes = this._ctrlModifier
        ? ctrlCharToByte(char)
        : new Uint8Array([0x1b, char.charCodeAt(0)]);
      if (bytes) this.sendInput(this._sessionId, bytes);
      this.setCtrlModifier(false);
      this.setAltModifier(false);
      this._androidCompositionValue = "";
      this.resetCaptureField();
      return;
    }

    if (value.startsWith(oldValue)) {
      const added = value.slice(oldValue.length);
      if (added) {
        this.sendInput(
          this._sessionId,
          encoder.encode(added.replace(/\n/g, "\r")),
        );
      }
    } else if (oldValue.startsWith(value)) {
      const deleted = oldValue.length - value.length;
      for (let i = 0; i < deleted; i++) {
        this.sendInput(this._sessionId, new Uint8Array([0x7f]));
      }
    } else {
      // Replacement (autocorrect/suggestion).  Delete what we previously
      // forwarded and send the new value.
      for (let i = 0; i < oldValue.length; i++) {
        this.sendInput(this._sessionId, new Uint8Array([0x7f]));
      }
      if (value) {
        this.sendInput(
          this._sessionId,
          encoder.encode(value.replace(/\n/g, "\r")),
        );
      }
    }

    this._androidCompositionValue = value;
  }

  // --- Scroll surface ---
  //
  // The scrollback navigation is driven by native scroll on `scrollEl`:
  // a transparent overlay over the canvas containing a spacer sized so its
  // reachable scroll range is (scrollback_lines * cell.h). Wheel and touch
  // gestures over the terminal therefore produce native scroll events with
  // momentum on mobile and OS-consistent feel on desktop.
  //
  // Mapping:
  //   scrollTop = (scrollback_lines - scrollOffset) * cell.h
  // i.e. scrollTop=max → newest output (scrollOffset=0); scrollTop=0 →
  // oldest in scrollback. The user therefore swipes UP / wheels UP to
  // travel back in time, matching every other scrollable surface.

  private setupScrollSurface(): void {
    const el = this.scrollEl;
    if (!el) return;
    this.boundScrollListener = () => {
      if (this.suppressScrollSync) return;
      const t = this.terminal;
      if (!t) return;
      const maxLines = t.scrollback_lines();
      const cellH = Math.max(1, this.cell.h);
      // Anchor on the distance to the *bottom*, measured from real DOM
      // geometry rather than recomputed as `maxLines * cellH`. The two
      // agree only while the spacer matches the current `clientHeight`;
      // between a pane resizing and the next render they do not, and the
      // recomputed form silently mapped a stale scrollTop onto the wrong
      // line. scrollTop=max → offset 0 (newest); scrollTop=0 → maxLines.
      // Falls back to the model when the element has no layout yet (before
      // the first frame, and under jsdom), where the measurement is 0 and
      // would read as "at the bottom".
      // A scroll callback also runs after layout, so refresh the cache from
      // real geometry while it is free.
      this.scrollViewH = el.clientHeight;
      this.lastScrollTop = el.scrollTop;
      this.lastUserScrollAt = performance.now();
      const measured = el.scrollHeight - el.clientHeight;
      const maxScrollTop = measured > 0 ? measured : maxLines * cellH;
      const fromBottom = maxScrollTop - el.scrollTop;
      const next = Math.max(
        0,
        Math.min(maxLines, Math.round(fromBottom / cellH)),
      );
      if (next === this.scrollOffset) return;
      const moved = next - this.scrollOffset;
      this.scrollOffset = next;
      if (this._sessionId !== null && this.status === "connected") {
        // `next` is absolute in *our* frame, which is one round trip behind
        // the server's whenever the app is printing. The gesture that
        // produced it is not.
        this.sendScrollBy(this._sessionId, this.scrollOffset, moved);
      }
      if (this.scrollOffset > 0) this.flashScrollbar();
      this.scheduleRender();
    };
    el.addEventListener("scroll", this.boundScrollListener, { passive: true });
    // Seed the cache: the first sync cannot measure it itself.
    this.scrollViewH = el.clientHeight;
    this.lastScrollTop = el.scrollTop;
    this.syncScrollSurface(/* preserveOffset */ false);
  }

  private teardownScrollSurface(): void {
    if (this.scrollEl && this.boundScrollListener) {
      this.scrollEl.removeEventListener("scroll", this.boundScrollListener);
    }
    this.boundScrollListener = null;
  }

  /**
   * Resize the spacer so the scroll range matches the current scrollback
   * depth, and align scrollEl.scrollTop with this.scrollOffset.
   *
   * Called from the render loop (cheap idempotent updates) and whenever
   * scrollOffset changes from a non-scroll source (e.g. Shift+PageUp).
   */
  private syncScrollSurface(preserveOffset: boolean): void {
    const el = this.scrollEl;
    const spacer = this.scrollSpacer;
    const t = this.terminal;
    if (!el || !spacer || !t) return;
    const cellH = Math.max(1, this.cell.h);
    const lines = t.scrollback_lines();
    // Browser scrollTop is capped at scrollHeight - clientHeight. Size the
    // content to viewport + scrollback range so the maximum reachable
    // scrollTop is exactly (scrollback_lines * cellH), matching the mapping
    // above and allowing offset 0 to land at native bottom.
    // The cached height, not `el.clientHeight`. This runs inside the render
    // loop, *after* the canvas style writes above, so reading layout here
    // forced a synchronous full-document reflow — once per pane, every
    // frame. During a window drag that was the dominant cost of resizing
    // (a profile put ~69% of the time in Layout with almost no JS
    // self-time, which is the signature of forced reflow rather than slow
    // script). The ResizeObserver and scroll callbacks both run after
    // layout, so measuring there is free.
    // Falls back to a real read only when nothing has cached a measurement
    // yet — before the first observer callback, and under jsdom. That is a
    // one-off; the steady state never reads layout here, which is the whole
    // point.
    const viewH = this.scrollViewH > 0 ? this.scrollViewH : el.clientHeight;
    const desired = `${viewH + lines * cellH}px`;
    if (spacer.style.height !== desired) spacer.style.height = desired;
    // Clamp scrollOffset to the (possibly shrunken) range first.
    if (preserveOffset) {
      this.scrollOffset = Math.max(0, Math.min(lines, this.scrollOffset));
    }
    const targetTop = (lines - this.scrollOffset) * cellH;
    // Compared against what we last wrote rather than read back from the
    // element, for the same reason: a read here is a forced reflow.
    const drift = Math.abs(this.lastScrollTop - targetTop);
    const anchorPx = Math.abs(this.anchorRowsSinceSync) * cellH;
    this.anchorRowsSinceSync = 0;
    if (
      drift > 0.5 &&
      drift <= anchorPx + cellH &&
      performance.now() - this.lastUserScrollAt < SCROLL_SETTLE_MS
    ) {
      // Mid-gesture and the disagreement is fully explained by the server's
      // re-anchoring (plus the usual sub-row snap): once the scrollback is
      // capped, every tick moves targetTop by whole rows, and writing them
      // back cancels the browser's momentum animation each time — the
      // "jumps". Fold the compensation into the bookkeeping instead; the
      // next genuine scroll event refreshes lastScrollTop from real DOM
      // geometry and re-derives the offset, so nothing is lost.
      this.lastScrollTop = targetTop;
      return;
    }
    if (drift > 0.5 && !this.gestureOwnsScrollTop(drift, cellH)) {
      this.lastScrollTop = targetTop;
      this.suppressScrollSync = true;
      el.scrollTop = targetTop;
      // The scroll event is async; clear the flag in the next frame.
      requestAnimationFrame(() => {
        this.suppressScrollSync = false;
      });
    }
  }

  /**
   * True when a scroll gesture is still in flight and the only disagreement
   * is where inside a row it stopped.
   *
   * `scrollOffset` is whole lines, so the position it maps back to is the
   * nearest row boundary — never more than half a row from wherever the
   * user actually is. Writing that back mid-gesture cancels the browser's
   * momentum animation and restarts it from a snapped position, once per
   * frame, which is what made a flick stutter. The offset the scroll
   * listener derived is already correct either way; the write is only a
   * cosmetic re-alignment, so it can wait for the gesture to end.
   *
   * A jump from somewhere else — Shift+PageUp, a paste, the server
   * re-anchoring a scrolled view — moves by rows, not by a fraction of
   * one, and still lands immediately.
   */
  private gestureOwnsScrollTop(drift: number, cellH: number): boolean {
    return (
      drift < cellH &&
      performance.now() - this.lastUserScrollAt < SCROLL_SETTLE_MS
    );
  }

  // --- Mouse input ---

  private setupMouse(): void {
    const canvas = this.glCanvas;
    const target = this.scrollEl;
    if (!canvas || !target || this._readOnly) return;

    const SCROLLBAR_HIT_PX = 20;
    const WORD_CHARS = /[A-Za-z0-9_\-./~:@]/;
    const URL_RE = /https?:\/\/[^\s<>"'`)\]},;]+/g;
    const AUTO_SCROLL_INTERVAL_MS = 50;
    const AUTO_SCROLL_LINES = 3;

    let mouseDownButton = -1;
    let lastMouseCell = { row: -1, col: -1 };
    let selecting = false;
    let selGranularity: 1 | 2 | 3 = 1;
    let autoScrollTimer: ReturnType<typeof setInterval> | null = null;
    let autoScrollDir: -1 | 0 | 1 = 0;
    let lastHoverUrl: string | null = null;

    const mouseToCell = (e: MouseEvent) => {
      const rect = this.canvasBox();
      if (!rect) return { row: 0, col: 0 };
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
        this.sendScrollBy(this._sessionId!, next, next - prev);
        this.flashScrollbar();
        const edgeRow = dir === 1 ? 0 : this._rows - 1;
        const edgeCol = dir === 1 ? 0 : this._cols - 1;
        const edgeSel = cellToSel({ row: edgeRow, col: edgeCol });
        if (selGranularity >= 2 && this.selAnchorStart && this.selAnchorEnd) {
          const { start: dragStart, end: dragEnd } =
            applyGranularitySel(edgeSel);
          if (selPosBefore(dragStart, this.selAnchorStart)) {
            this.selStart = dragStart;
            this.selEnd = this.selAnchorEnd;
          } else {
            this.selStart = this.selAnchorStart;
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

    const urlAt = (row: number, col: number): UrlHit | null => {
      // An explicit OSC 8 hyperlink wins over regex detection: the application
      // said where the text points, and the visible text may be nothing like
      // the target. `has_links()` keeps the common link-free frame on the
      // cheap path — no per-cell probing across the WASM boundary.
      const t = this.terminal;
      if (t?.has_links()) {
        const url = t.link_at(row, col);
        if (url !== undefined && url !== null) {
          // Flat [row, startCol, endCol] triples, one per row the link spans.
          const flat = t.link_segments(row, col);
          const segments: LinkSegment[] = [];
          for (let i = 0; i + 2 < flat.length; i += 3) {
            segments.push({
              row: flat[i],
              startCol: flat[i + 1],
              endCol: flat[i + 2],
            });
          }
          return { url, segments, explicit: true };
        }
      }

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
          return {
            url: raw,
            segments: [{ row, startCol, endCol }],
            explicit: false,
          };
      }
      return null;
    };

    const handleMouseDown = (e: MouseEvent) => {
      if (e.button === 0 && this.scrollbarGeo && isNearScrollbar(e)) {
        e.preventDefault();
        const geo = this.scrollbarGeo;
        const y = canvasYFromEvent(e);
        this.scrollDragging = true;
        this.setCursor(target, "grabbing");
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
        const cell = mouseToCell(e);
        const sel = cellToSel(cell);
        const detail = Math.min(e.detail, 3) as 1 | 2 | 3;
        selGranularity = detail;
        if (detail >= 2) {
          const { start, end } = applyGranularitySel(sel);
          this.selStart = start;
          this.selEnd = end;
          this.selAnchorStart = start;
          this.selAnchorEnd = end;
          this.scheduleRender();
        } else {
          this.selStart = sel;
          this.selEnd = sel;
          this.selAnchorStart = null;
          this.selAnchorEnd = null;
        }
      }
    };

    const handleMouseMove = (e: MouseEvent) => {
      if (this.scrollDragging) {
        scrollToCanvasY(canvasYFromEvent(e) - this.scrollDragOffset);
        return;
      }
      const overCanvas =
        mouseDownButton >= 0 || target.contains(e.target as Node);
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
        if (selGranularity >= 2 && this.selAnchorStart && this.selAnchorEnd) {
          const { start: dragStart, end: dragEnd } = applyGranularitySel(sel);
          if (selPosBefore(dragStart, this.selAnchorStart)) {
            this.selStart = dragStart;
            this.selEnd = this.selAnchorEnd;
          } else {
            this.selStart = this.selAnchorStart;
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
        this.setCursor(target, "text");
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
      if (target.contains(e.target as Node)) {
        this.inputEl?.focus();
      }
    };

    // Mouse-reporting apps scroll themselves, so the wheel has to reach them
    // as the discrete steps they expect — one per detent, three lines' worth
    // of travel each. One report per DOM event instead let a trackpad, which
    // emits an event per frame, scroll such an app about twenty times faster
    // than the same flick moves anything else on screen.
    const wheelDetents = new WheelDetents();
    const handleCanvasWheel = (e: WheelEvent) => {
      const t = this.terminal;
      if (!t || t.mouse_mode() === 0 || e.shiftKey) return;
      // Ctrl+wheel is how browsers report a pinch-zoom, including macOS
      // trackpad pinches. It is a zoom request, not a scroll.
      if (e.ctrlKey) return;
      // Claim the gesture even when it hasn't completed a step yet, or the
      // leftover travel scrolls our own scrollback at the same time.
      e.preventDefault();
      const steps = wheelDetents.take(
        e,
        this.cell.h,
        this._rows,
        performance.now(),
      );
      // Sideways travel lands here with deltaY at 0 and produces no steps.
      // It used to report a wheel-*down* per event, so a horizontal swipe
      // scrolled the app.
      if (steps === 0) return;
      const button = steps < 0 ? 64 : 65;
      for (let i = Math.abs(steps); i > 0; i--) {
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
          this.activateLink(hit);
          return;
        }
      }
      this.inputEl?.focus();
    };

    const handleHoverMove = (e: MouseEvent) => {
      if (this.scrollDragging) {
        this.setCursor(target, "grabbing");
        return;
      }
      if (this.scrollbarGeo && isNearScrollbar(e)) {
        this.setCursor(target, "default");
        return;
      }
      if (selecting) {
        if (this.hoveredUrl) {
          this.hoveredUrl = null;
          this.scheduleRender();
          this.setCursor(target, "text");
          lastHoverUrl = null;
        }
        return;
      }
      const cell = mouseToCell(e);
      const hit = urlAt(cell.row, cell.col);
      const url = hit?.url ?? null;
      if (url !== lastHoverUrl) {
        lastHoverUrl = url;
        // A link we would refuse to open must not present itself as clickable.
        const assessment = hit ? assessUrl(hit.url) : null;
        this.setCursor(
          target,
          assessment && assessment.verdict !== "deny" ? "pointer" : "text",
        );
        this.hoveredUrl =
          hit && assessment
            ? { segments: hit.segments, url: hit.url, assessment }
            : null;
        this.emitLinkHover(
          hit && assessment
            ? {
                assessment,
                explicit: hit.explicit,
                // Joined across wrapped rows so the text matches what the user
                // reads, not just the row the pointer happens to be on.
                text: hit.segments
                  .map((s) => getRowText(s.row).slice(s.startCol, s.endCol + 1))
                  .join(""),
              }
            : null,
        );
        this.scheduleRender();
      }
    };

    /**
     * Forget the hovered link.
     *
     * `handleHoverMove` only fires while the pointer is over the surface, so
     * every way of stopping being over a link *without* crossing another cell
     * first — leaving the element, the window losing focus, the pane being
     * torn down — has to say so explicitly. Otherwise the status-bar preview
     * outlives the thing it describes and sits on top of the focused pane's
     * identity indefinitely.
     *
     * `lastHoverUrl` is reset too, or re-entering the same link would compare
     * equal and never re-emit. That in turn is why the cursor is reset here:
     * coming back onto a *non*-link cell computes `null !== null`, takes the
     * unchanged path, and would otherwise keep the pointer cursor.
     *
     * `redraw` is false during teardown, where there is no surface left to
     * draw into — `dispose()` detaches before it sets `disposed`, so
     * `scheduleRender` would still queue a frame.
     */
    const clearHover = (redraw: boolean) => {
      lastHoverUrl = null;
      if (!this.hoveredUrl) return;
      this.hoveredUrl = null;
      this.emitLinkHover(null);
      if (redraw) {
        this.setCursor(target, "text");
        this.scheduleRender();
      }
    };

    /**
     * Also bound to `scroll`: content moving under a stationary pointer fires
     * no `mousemove`, so the preview would keep naming a link that has since
     * scrolled elsewhere. A click re-runs the hit test and so stays correct,
     * but a preview that disagrees with what the click would open is exactly
     * the confusion the preview exists to prevent. Dropping it is the honest
     * answer — the next pointer move re-establishes it.
     */
    const handleHoverInvalidated = () => clearHover(true);

    const handleBlur = () => {
      clearHover(true);
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
      this.touchSelAnchor = sel;
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
          this.touchSelAnchor = null;
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
      if (touchSelecting && this.touchSelAnchor) {
        e.preventDefault();
        const cell = mouseToCell(
          new MouseEvent("touch", {
            clientX: touch.clientX,
            clientY: touch.clientY,
          }),
        );
        const sel = cellToSel(cell);
        if (selPosBefore(sel, this.touchSelAnchor)) {
          this.selStart = sel;
          this.selEnd = this.touchSelAnchor;
        } else {
          this.selStart = this.touchSelAnchor;
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

      const t = this.terminal;
      // Mouse-reporting apps (htop, vim, …) need wheel-button events for
      // their internal scrolling. Native browser scroll would silently
      // swallow those gestures, so we synthesise wheel reports per cell-
      // height of vertical movement and preventDefault to stop the
      // browser from also scrolling the surface.
      if (t && t.mouse_mode() > 0) {
        const dy = touchLastY - touch.clientY;
        touchLastY = touch.clientY;
        touchAccum += dy;
        const lineH = this.cell.h || 20;
        // Every report carries the cell where the drag began: the first
        // wheel step places the app's cursor there (vim & co. move the
        // cursor to the reported position), and pinning the rest keeps a
        // sideways-drifting finger from walking the cursor mid-scroll.
        const pos = mouseToCell(
          new MouseEvent("wheel", {
            clientX: touchStartX,
            clientY: touchStartY,
          }),
        );
        while (Math.abs(touchAccum) >= lineH) {
          touchScrolled = true;
          const dir = touchAccum > 0 ? 1 : -1;
          touchAccum -= dir * lineH;
          // Natural touch semantics: a finger dragging the content up
          // (dir > 0) reveals what's below, i.e. wheel-down (65) — the
          // same sign convention as the finger scroll in BlitSurfaceCanvas.
          const button = dir > 0 ? 65 : 64;
          this._workspace?.sendMouse(
            this._sessionId!,
            MOUSE_DOWN,
            button,
            pos.col,
            pos.row,
          );
          e.preventDefault();
        }
        return;
      }

      // Normal mode: vertical pan is handled by native scroll on
      // `scrollEl` (touch-action: pan-y). Just track that the gesture is
      // a scroll so touchend doesn't synthesise a tap.
      const dyAbsTotal = Math.abs(touch.clientY - touchStartY);
      if (dyAbsTotal > LONG_PRESS_SLOP_PX) touchScrolled = true;
      touchLastY = touch.clientY;
    };

    const handleTouchEnd = (e: TouchEvent) => {
      for (let i = 0; i < e.changedTouches.length; i++) {
        if (e.changedTouches[i]!.identifier === touchId) {
          cancelLongPress();
          touchId = null;
          touchAccum = 0;
          if (touchSelecting) {
            // Auto-copy the freshly built selection while the user gesture
            // is still live for navigator.clipboard.writeText. Synchronous
            // for in-viewport selections (the common touch case), so the
            // clipboard write fires before the gesture token expires.
            void this.copySelection();
            touchSelecting = false;
            this.touchSelAnchor = null;
            // Suppress the synthetic mousedown/click iOS dispatches after
            // a long-press touch sequence, otherwise our mouse handler
            // would clear the freshly built selection.
            e.preventDefault();
          }
          break;
        }
      }
    };

    target.addEventListener("touchstart", handleTouchStart, { passive: true });
    target.addEventListener("touchmove", handleTouchMove, { passive: false });
    target.addEventListener("touchend", handleTouchEnd, { passive: false });
    target.addEventListener("touchcancel", handleTouchEnd, { passive: false });

    target.addEventListener("mousedown", handleMouseDown);
    window.addEventListener("mousemove", handleMouseMove);
    target.addEventListener("mousemove", handleHoverMove);
    target.addEventListener("mouseleave", handleHoverInvalidated);
    target.addEventListener("scroll", handleHoverInvalidated, {
      passive: true,
    });
    window.addEventListener("mouseup", handleMouseUp);
    window.addEventListener("blur", handleBlur);
    target.addEventListener("wheel", handleCanvasWheel, { passive: false });
    target.addEventListener("contextmenu", handleContextMenu);
    target.addEventListener("click", handleClick);

    this.mouseCleanup = () => {
      target.removeEventListener("touchstart", handleTouchStart);
      target.removeEventListener("touchmove", handleTouchMove);
      target.removeEventListener("touchend", handleTouchEnd);
      target.removeEventListener("touchcancel", handleTouchEnd);
      target.removeEventListener("mousedown", handleMouseDown);
      window.removeEventListener("mousemove", handleMouseMove);
      target.removeEventListener("mousemove", handleHoverMove);
      target.removeEventListener("mouseleave", handleHoverInvalidated);
      target.removeEventListener("scroll", handleHoverInvalidated);
      window.removeEventListener("mouseup", handleMouseUp);
      window.removeEventListener("blur", handleBlur);
      target.removeEventListener("wheel", handleCanvasWheel);
      target.removeEventListener("contextmenu", handleContextMenu);
      target.removeEventListener("click", handleClick);
      // After the listeners, so nothing can re-establish it, but while the
      // hover listeners are still subscribed so the host clears its preview.
      clearHover(false);
      if (this.scrollFadeTimer) clearTimeout(this.scrollFadeTimer);
      stopAutoScroll();
    };
  }

  private teardownMouse(): void {
    this.mouseCleanup?.();
    this.mouseCleanup = null;
    // Drop any in-progress selection state with the handlers that own it.
    this.clearSelection();
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

  /**
   * Report a scroll the user *moved* rather than one they aimed at.
   *
   * Everything incremental — a wheel notch, a page key, a selection drag
   * running off the edge — belongs here: the absolute offset it works out
   * to counts from a live bottom that the app may move before the message
   * lands, and the server re-anchors us in that same window. Sent as a
   * relative move, the two compose instead of racing. `offset` rides along
   * for servers that only know the absolute form.
   */
  private sendScrollBy(
    sessionId: SessionId,
    offset: number,
    lines: number,
  ): void {
    this._workspace?.scrollSessionBy(sessionId, offset, lines);
  }
}
