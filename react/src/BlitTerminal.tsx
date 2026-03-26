import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import type { Terminal } from "blit-browser";
import type {
  BlitTerminalProps,
  ConnectionStatus,
  TerminalPalette,
} from "./types";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import {
  buildInputMessage,
  buildResizeMessage,
  buildScrollMessage,
} from "./protocol";
import { measureCell, type CellMetrics } from "./hooks/useBlitTerminal";
import { useBlitContext } from "./BlitContext";
import { createGlRenderer, type GlRenderer } from "./gl-renderer";
import { keyToBytes, encoder } from "./keyboard";

// ---------------------------------------------------------------------------
// Public handle exposed via ref
// ---------------------------------------------------------------------------

export interface BlitTerminalHandle {
  /** The underlying WASM Terminal instance, if initialised. */
  terminal: Terminal | null;
  /** Current grid dimensions. */
  rows: number;
  cols: number;
  /** Current connection status. */
  status: ConnectionStatus;
  /** Focus the input sink so the terminal can receive keyboard events. */
  focus(): void;
}

// ---------------------------------------------------------------------------
// Component
// ---------------------------------------------------------------------------

/**
 * BlitTerminal renders a blit terminal inside a WebGL canvas.
 *
 * It handles WASM initialisation, server message processing, GL rendering,
 * keyboard/mouse input, and dynamic resizing through a ResizeObserver.
 */
export const BlitTerminal = forwardRef<BlitTerminalHandle, BlitTerminalProps>(
  function BlitTerminal(props, ref) {
    const ctx = useBlitContext();
    const store = props.store ?? ctx.store;
    if (!store) {
      throw new Error(
        "BlitTerminal requires a store prop or a BlitProvider ancestor",
      );
    }
    const transport = props.transport ?? ctx.transport ?? store.transport;
    const {
      ptyId,
      fontFamily = ctx.fontFamily ?? DEFAULT_FONT,
      fontSize = ctx.fontSize ?? DEFAULT_FONT_SIZE,
      className,
      style,
      palette = ctx.palette,
      readOnly = false,
    } = props;

    // Refs for DOM elements.
    const containerRef = useRef<HTMLDivElement>(null);
    const glCanvasRef = useRef<HTMLCanvasElement>(null);
    const overlayCanvasRef = useRef<HTMLCanvasElement>(null);
    const inputRef = useRef<HTMLTextAreaElement>(null);

    // Refs for mutable state that must not trigger re-renders.
    const terminalRef = useRef<Terminal | null>(null);
    const rendererRef = useRef<GlRenderer | null>(null);
    const rafRef = useRef<number>(0);
    const cellRef = useRef<CellMetrics>(measureCell(fontFamily, fontSize));
    const rowsRef = useRef(24);
    const colsRef = useRef(80);
    const needsRenderRef = useRef(false);

    const scrollOffsetRef = useRef(0);
    const cursorBlinkOnRef = useRef(true);
    const cursorBlinkTimerRef = useRef<ReturnType<typeof setInterval> | null>(
      null,
    );
    const paletteRef = useRef<TerminalPalette | undefined>(palette);


    const selStartRef = useRef<{ row: number; col: number } | null>(null);
    const selEndRef = useRef<{ row: number; col: number } | null>(null);
    const hoveredUrlRef = useRef<{
      row: number;
      startCol: number;
      endCol: number;
      url: string;
    } | null>(null);

    const predictedRef = useRef("");
    const predictedFromRowRef = useRef(0);
    const predictedFromColRef = useRef(0);

    // React state for things the consumer might read.
    const [wasmReady, setWasmReady] = useState(false);

    // -----------------------------------------------------------------------
    // Connection callbacks — BlitTerminal only cares about UPDATE (rendering)
    // -----------------------------------------------------------------------

    const [status, setStatus] = useState<ConnectionStatus>(transport.status);

    const reconcilePrediction = useCallback(() => {
      const t = terminalRef.current;
      if (!t || !predictedRef.current) return;
      const cr = t.cursor_row;
      const cc = t.cursor_col;
      if (cr !== predictedFromRowRef.current) {
        predictedRef.current = "";
        return;
      }
      const advance = cc - predictedFromColRef.current;
      if (advance > 0 && advance <= predictedRef.current.length) {
        predictedRef.current = predictedRef.current.slice(advance);
        predictedFromColRef.current = cc;
      } else if (advance < 0 || advance > predictedRef.current.length) {
        predictedRef.current = "";
      }
    }, []);

    const applyPaletteToTerminal = useCallback((t: Terminal | null) => {
      const nextPalette = paletteRef.current;
      if (!t || !nextPalette) return;
      t.set_default_colors(...nextPalette.fg, ...nextPalette.bg);
      for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...nextPalette.ansi[i]);
      needsRenderRef.current = true;
    }, []);

    useEffect(() => {
      const syncReadOnlySize = (t: Terminal) => {
        const tr = t.rows;
        const tc = t.cols;
        if (tr !== rowsRef.current || tc !== colsRef.current) {
          rowsRef.current = tr;
          colsRef.current = tc;
        }
        needsRenderRef.current = true;
      };
      const unsub = store.addDirtyListener((dirtyPtyId: number) => {
        if (dirtyPtyId !== ptyId) return;
        const t = store.getTerminal(dirtyPtyId);
        if (!t) return;
        if (terminalRef.current !== t) {
          terminalRef.current = t;
          applyPaletteToTerminal(t);
        }
        needsRenderRef.current = true;
        reconcilePrediction();
        if (readOnly) syncReadOnlySize(t);
      });
      if (ptyId !== null) {
        const t = store.getTerminal(ptyId);
        if (t) {
          terminalRef.current = t;
          applyPaletteToTerminal(t);
          if (readOnly) syncReadOnlySize(t);
        }
      }
      const onStatus = (s: ConnectionStatus) => setStatus(s);
      transport.addEventListener("statuschange", onStatus);
      setStatus(transport.status);
      return () => {
        unsub();
        transport.removeEventListener("statuschange", onStatus);
      };
    }, [transport, ptyId, readOnly, store, reconcilePrediction, applyPaletteToTerminal]);

    const sendInput = useCallback(
      (id: number, data: Uint8Array) => {
        transport.send(buildInputMessage(id, data));
      },
      [transport],
    );

    const sendResize = useCallback(
      (id: number, rows: number, cols: number) => {
        transport.send(buildResizeMessage(id, rows, cols));
      },
      [transport],
    );

    const sendScroll = useCallback(
      (id: number, offset: number) => {
        transport.send(buildScrollMessage(id, offset));
      },
      [transport],
    );



    // -----------------------------------------------------------------------
    // Imperative handle
    // -----------------------------------------------------------------------

    useImperativeHandle(
      ref,
      () => ({
        get terminal() {
          return terminalRef.current;
        },
        get rows() {
          return rowsRef.current;
        },
        get cols() {
          return colsRef.current;
        },
        status,
        focus() {
          inputRef.current?.focus();
        },
      }),
      [status],
    );

    // -----------------------------------------------------------------------
    // WASM init
    // -----------------------------------------------------------------------

    useEffect(() => {
      const unsub = store.onReady(() => setWasmReady(true));
      if (store.isReady()) setWasmReady(true);
      return unsub;
    }, [store]);

    // -----------------------------------------------------------------------
    // Cell measurement (re-measure when font changes)
    // -----------------------------------------------------------------------

    useEffect(() => {
      const cell = measureCell(fontFamily, fontSize);
      cellRef.current = cell;
      if (!readOnly) {
        const t = terminalRef.current;
        if (t) {
          t.set_cell_size(cell.pw, cell.ph);
          t.set_font_family(fontFamily);
        }
        store.setCellSize(cell.pw, cell.ph);
      }
      needsRenderRef.current = true;
    }, [fontFamily, fontSize, store, readOnly]);

    // -----------------------------------------------------------------------
    // Cursor blink timer
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (readOnly) return;
      cursorBlinkOnRef.current = true;
      const timer = setInterval(() => {
        cursorBlinkOnRef.current = !cursorBlinkOnRef.current;
        needsRenderRef.current = true;
      }, 530);
      cursorBlinkTimerRef.current = timer;
      return () => {
        clearInterval(timer);
        cursorBlinkTimerRef.current = null;
      };
    }, [readOnly]);

    // -----------------------------------------------------------------------
    // GL renderer lifecycle
    // -----------------------------------------------------------------------

    useEffect(() => {
      const canvas = glCanvasRef.current;
      if (!canvas) return;
      const renderer = createGlRenderer(canvas);
      rendererRef.current = renderer;
      return () => {
        renderer.dispose();
        rendererRef.current = null;
      };
    }, []);

    // -----------------------------------------------------------------------
    // Terminal instance lifecycle
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (ptyId !== null) {
        store.retain(ptyId);
        const t = store.getTerminal(ptyId);
        if (t) {
          terminalRef.current = t;
          applyPaletteToTerminal(t);
          if (!readOnly) {
            const cell = cellRef.current;
            t.set_cell_size(cell.pw, cell.ph);
            t.set_font_family(fontFamily);
          }
          needsRenderRef.current = true;
        }
      } else {
        terminalRef.current = null;
      }
      return () => {
        terminalRef.current = null;
        needsRenderRef.current = false;
        if (ptyId !== null) store.release(ptyId);
      };
    }, [wasmReady, ptyId, store, fontFamily, readOnly, applyPaletteToTerminal]);

    // -----------------------------------------------------------------------
    // Palette changes
    // -----------------------------------------------------------------------

    useEffect(() => {
      paletteRef.current = palette;
      applyPaletteToTerminal(terminalRef.current);
    }, [palette, applyPaletteToTerminal]);

    // -----------------------------------------------------------------------
    // Resize observer
    // -----------------------------------------------------------------------

    useEffect(() => {
      const container = containerRef.current;
      if (!container || readOnly) return;

      let resizeTimer: ReturnType<typeof setTimeout> | null = null;
      let pendingRows = rowsRef.current;
      let pendingCols = colsRef.current;
      let lastSentPtyId: number | null = null;

      const flushResize = () => {
        resizeTimer = null;
        if (ptyId !== null) {
          sendResize(ptyId, pendingRows, pendingCols);
        }
      };

      const handleResize = () => {
        const cell = cellRef.current;
        const w = container.clientWidth;
        const h = container.clientHeight;
        const cols = Math.max(1, Math.floor(w / cell.w));
        const rows = Math.max(1, Math.floor(h / cell.h));

        const sizeChanged = cols !== colsRef.current || rows !== rowsRef.current;
        if (sizeChanged) {
          rowsRef.current = rows;
          colsRef.current = cols;
          needsRenderRef.current = true;
        }

        pendingRows = rows;
        pendingCols = cols;

        // Send immediately on ptyId change, debounce during continuous resize
        if (ptyId !== null && lastSentPtyId !== ptyId) {
          lastSentPtyId = ptyId;
          sendResize(ptyId, rows, cols);
        } else if (sizeChanged) {
          if (resizeTimer) clearTimeout(resizeTimer);
          resizeTimer = setTimeout(flushResize, 100);
        }
      };

      const observer = new ResizeObserver(handleResize);
      observer.observe(container);
      window.addEventListener("resize", handleResize);
      handleResize();

      return () => {
        observer.disconnect();
        window.removeEventListener("resize", handleResize);
        if (resizeTimer) clearTimeout(resizeTimer);
      };
    }, [ptyId, readOnly, sendResize]);

    // -----------------------------------------------------------------------
    // Render loop
    // -----------------------------------------------------------------------

    useEffect(() => {
      let running = true;

      const renderLoop = () => {
        if (!running) return;

        if (
          needsRenderRef.current &&
          terminalRef.current &&
          rendererRef.current?.supported
        ) {
          needsRenderRef.current = false;
          const t = terminalRef.current;
          // Guard against freed WASM objects (PTY closed or switched)
          const cell = cellRef.current;
          const renderer = rendererRef.current;

          // Always sync canvas backing to the terminal's actual grid.
          // The terminal dimensions come from the server and may differ
          // from colsRef/rowsRef (which reflect the container size).
          {
            const termCols = t.cols;
            const termRows = t.rows;
            const pw = termCols * cell.pw;
            const ph = termRows * cell.ph;
            if (!readOnly) {
              const cssW = `${termCols * cell.w}px`;
              const cssH = `${termRows * cell.h}px`;
              const glCanvas = glCanvasRef.current;
              if (glCanvas) {
                if (glCanvas.style.width !== cssW) glCanvas.style.width = cssW;
                if (glCanvas.style.height !== cssH) glCanvas.style.height = cssH;
              }
              const overlay = overlayCanvasRef.current;
              if (overlay) {
                if (overlay.style.width !== cssW) overlay.style.width = cssW;
                if (overlay.style.height !== cssH) overlay.style.height = cssH;
              }
            }
            renderer.resize(pw, ph);
            const overlay = overlayCanvasRef.current;
            if (!readOnly && overlay) {
              if (overlay.width !== pw) overlay.width = pw;
              if (overlay.height !== ph) overlay.height = ph;
            }
          }

          t.prepare_render_ops();
          renderer.render(
            t.background_ops(),
            t.glyph_ops(),
            t.glyph_atlas_canvas(),
            t.cursor_visible(),
            t.cursor_col,
            t.cursor_row,
            t.cursor_style(),
            cursorBlinkOnRef.current,
            cell,
            paletteRef.current?.bg ?? [0, 0, 0],
          );

          // Overflow text (emoji / wide Unicode) via 2D overlay canvas.
          const overflowCount = t.overflow_text_count();
          const overlay = overlayCanvasRef.current;
          if (overlay) {
            const ctx = overlay.getContext("2d");
            if (ctx) {
              ctx.clearRect(0, 0, overlay.width, overlay.height);

              // Selection highlight
              const ss = selStartRef.current;
              const se = selEndRef.current;
              if (ss && se) {
                let sr = ss.row, sc = ss.col;
                let er = se.row, ec = se.col;
                if (sr > er || (sr === er && sc > ec)) {
                  [sr, sc, er, ec] = [er, ec, sr, sc];
                }
                ctx.fillStyle = "rgba(100,150,255,0.3)";
                for (let r = sr; r <= er; r++) {
                  const c0 = r === sr ? sc : 0;
                  const c1 = r === er ? ec : colsRef.current - 1;
                  ctx.fillRect(
                    c0 * cell.pw,
                    r * cell.ph,
                    (c1 - c0 + 1) * cell.pw,
                    cell.ph,
                  );
                }
              }

              // URL hover underline
              const hurl = hoveredUrlRef.current;
              if (hurl) {
                const [fgR, fgG, fgB] = paletteRef.current?.fg ?? [204, 204, 204];
                ctx.strokeStyle = `rgba(${fgR},${fgG},${fgB},0.6)`;
                ctx.lineWidth = Math.max(1, Math.round(cell.ph * 0.06));
                const y = hurl.row * cell.ph + cell.ph - ctx.lineWidth;
                const x1 = hurl.startCol * cell.pw;
                const x2 = (hurl.endCol + 1) * cell.pw;
                ctx.beginPath();
                ctx.moveTo(x1, y);
                ctx.lineTo(x2, y);
                ctx.stroke();
              }

              if (overflowCount > 0) {
                const cw = cell.pw;
                const ch = cell.ph;
                const scale = 0.85;
                const scaledH = ch * scale;
                const fSize = Math.max(1, Math.round(scaledH));
                ctx.font = `${fSize}px ${fontFamily}`;
                ctx.textBaseline = "bottom";
                const [fgR, fgG, fgB] = paletteRef.current?.fg ?? [
                  204, 204, 204,
                ];
                ctx.fillStyle = `#${fgR.toString(16).padStart(2, "0")}${fgG.toString(16).padStart(2, "0")}${fgB.toString(16).padStart(2, "0")}`;
                for (let i = 0; i < overflowCount; i++) {
                  const op = t.overflow_text_op(i);
                  if (!op) continue;
                  const [row, col, colSpan, text] = op as [
                    number,
                    number,
                    number,
                    string,
                  ];
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
            }
          }

          if (!readOnly && predictedRef.current && overlay) {
            const t2 = terminalRef.current;
            if (t2 && t2.echo()) {
              const ctx2 = overlay.getContext("2d");
              if (ctx2) {
                const cw = cell.pw;
                const ch = cell.ph;
                const cc = t2.cursor_col;
                const cr = t2.cursor_row;
                const [fR, fG, fB] = paletteRef.current?.fg ?? [204, 204, 204];
                ctx2.fillStyle = `rgba(${fR},${fG},${fB},0.5)`;
                const fSize = Math.max(1, Math.round(ch * 0.85));
                ctx2.font = `${fSize}px ${fontFamily}`;
                ctx2.textBaseline = "bottom";
                const pred = predictedRef.current;
                for (let i = 0; i < pred.length && cc + i < colsRef.current; i++) {
                  ctx2.fillText(pred[i], (cc + i) * cw, cr * ch + ch);
                }
              }
            }
          }

        }

        rafRef.current = requestAnimationFrame(renderLoop);
      };

      rafRef.current = requestAnimationFrame(renderLoop);

      return () => {
        running = false;
        cancelAnimationFrame(rafRef.current);
      };
    }, [fontFamily, readOnly]);

    // -----------------------------------------------------------------------
    // Keyboard input
    // -----------------------------------------------------------------------

    useEffect(() => {
      const input = inputRef.current;
      if (!input || readOnly) return;

      const handleKeyDown = (e: KeyboardEvent) => {
        if (ptyId === null || status !== "connected") return;
        if (e.isComposing) return;
        if (e.key === "Dead") return;

        const t = terminalRef.current;
        const appCursor = t ? t.app_cursor() : false;
        const bytes = keyToBytes(e, appCursor);
        if (bytes) {
          e.preventDefault();
          if (scrollOffsetRef.current > 0) {
            scrollOffsetRef.current = 0;
            sendScroll(ptyId, 0);
          }
          if (t && t.echo() && e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
            if (!predictedRef.current) {
              predictedFromRowRef.current = t.cursor_row;
              predictedFromColRef.current = t.cursor_col;
            }
            predictedRef.current += e.key;
            needsRenderRef.current = true;
          } else {
            predictedRef.current = "";
          }
          sendInput(ptyId, bytes);
        }
      };

      const handleCompositionEnd = (e: CompositionEvent) => {
        if (e.data && ptyId !== null && status === "connected") {
          sendInput(ptyId, encoder.encode(e.data));
        }
        input.value = "";
      };

      const handleInput = (e: Event) => {
        const inputEvent = e as InputEvent;
        if (inputEvent.isComposing) {
          if (
            inputEvent.inputType === "deleteContentBackward" &&
            !input.value &&
            ptyId !== null &&
            status === "connected"
          ) {
            sendInput(ptyId, new Uint8Array([0x7f]));
          }
          return;
        }
        if (inputEvent.inputType === "deleteContentBackward" && !input.value) {
          if (ptyId !== null && status === "connected") {
            sendInput(ptyId, new Uint8Array([0x7f]));
          }
        } else if (input.value && ptyId !== null && status === "connected") {
          sendInput(ptyId, encoder.encode(input.value.replace(/\n/g, "\r")));
        }
        input.value = "";
      };

      input.addEventListener("keydown", handleKeyDown);
      input.addEventListener("compositionend", handleCompositionEnd);
      input.addEventListener("input", handleInput);

      return () => {
        input.removeEventListener("keydown", handleKeyDown);
        input.removeEventListener("compositionend", handleCompositionEnd);
        input.removeEventListener("input", handleInput);
      };
    }, [ptyId, status, readOnly, sendInput, sendScroll]);

    // -----------------------------------------------------------------------
    // Mouse input
    // -----------------------------------------------------------------------

    useEffect(() => {
      const canvas = glCanvasRef.current;
      if (!canvas || readOnly) return;

      function mouseToCell(e: MouseEvent): { row: number; col: number } {
        const rect = canvas!.getBoundingClientRect();
        const cell = cellRef.current;
        return {
          row: Math.min(
            Math.max(Math.floor((e.clientY - rect.top) / cell.h), 0),
            rowsRef.current - 1,
          ),
          col: Math.min(
            Math.max(Math.floor((e.clientX - rect.left) / cell.w), 0),
            colsRef.current - 1,
          ),
        };
      }

      function sendMouseEvent(
        type: "down" | "up" | "move",
        e: MouseEvent,
        button: number,
      ): boolean {
        const t = terminalRef.current;
        if (
          !t ||
          t.mouse_mode() === 0 ||
          ptyId === null ||
          status !== "connected"
        )
          return false;

        const pos = mouseToCell(e);
        const enc = t.mouse_encoding();

        if (enc === 2) {
          const suffix = type === "up" ? "m" : "M";
          const seq = `\x1b[<${button};${pos.col + 1};${pos.row + 1}${suffix}`;
          sendInput(ptyId, encoder.encode(seq));
        } else {
          if (type === "up") button = 3;
          const seq = new Uint8Array([
            0x1b,
            0x5b,
            0x4d,
            button + 32,
            pos.col + 33,
            pos.row + 33,
          ]);
          sendInput(ptyId, seq);
        }
        return true;
      }

      let selecting = false;
      // 1=char, 2=word, 3=line — set by click detail
      let selGranularity: 1 | 2 | 3 = 1;
      // Anchor word/line boundaries for drag-extending
      let selAnchorStart: { row: number; col: number } | null = null;
      let selAnchorEnd: { row: number; col: number } | null = null;

      function clearSelection() {
        selStartRef.current = selEndRef.current = null;
        needsRenderRef.current = true;
      }

      function drawSelection() {
        needsRenderRef.current = true;
      }

      function getRowText(row: number): string {
        const t = terminalRef.current;
        if (!t) return "";
        return t.get_text(row, 0, row, colsRef.current - 1);
      }

      const WORD_CHARS = /[A-Za-z0-9_\-./~:@]/;

      function wordBoundsAt(row: number, col: number): { start: number; end: number } {
        const text = getRowText(row);
        if (col >= text.length || !WORD_CHARS.test(text[col])) {
          return { start: col, end: col };
        }
        let start = col;
        while (start > 0 && WORD_CHARS.test(text[start - 1])) start--;
        let end = col;
        while (end < text.length - 1 && WORD_CHARS.test(text[end + 1])) end++;
        return { start, end };
      }

      function isWrapped(row: number): boolean {
        const t = terminalRef.current as Record<string, unknown> | null;
        if (t && typeof t.is_wrapped === "function") {
          return !!(t.is_wrapped as (row: number) => boolean)(row);
        }
        // Fallback for older WASM builds without is_wrapped
        const text = getRowText(row);
        return text.length >= colsRef.current;
      }

      function logicalLineRange(row: number): { startRow: number; endRow: number } {
        const maxRow = rowsRef.current - 1;
        let startRow = row;
        while (startRow > 0 && isWrapped(startRow - 1)) startRow--;
        let endRow = row;
        while (endRow < maxRow && isWrapped(endRow)) endRow++;
        return { startRow, endRow };
      }

      function applyGranularity(cell: { row: number; col: number }): {
        start: { row: number; col: number };
        end: { row: number; col: number };
      } {
        if (selGranularity === 3) {
          const { startRow, endRow } = logicalLineRange(cell.row);
          return {
            start: { row: startRow, col: 0 },
            end: { row: endRow, col: colsRef.current - 1 },
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
      }

      function copySelection() {
        if (!selStartRef.current || !selEndRef.current) return;
        const t = terminalRef.current;
        if (!t) return;
        let sr = selStartRef.current.row, sc = selStartRef.current.col;
        let er = selEndRef.current.row, ec = selEndRef.current.col;
        if (sr > er || (sr === er && sc > ec)) {
          [sr, sc, er, ec] = [er, ec, sr, sc];
        }
        const text = t.get_text(sr, sc, er, ec);
        const html = t.get_html(sr, sc, er, ec);
        if (text) {
          navigator.clipboard.write([
            new ClipboardItem({
              "text/plain": new Blob([text], { type: "text/plain" }),
              "text/html": new Blob([html], { type: "text/html" }),
            }),
          ]);
        }
      }

      const handleMouseDown = (e: MouseEvent) => {
        if (!e.shiftKey && sendMouseEvent("down", e, e.button)) {
          e.preventDefault();
          return;
        }
        if (e.button === 0) {
          clearSelection();
          selecting = true;
          const cell = mouseToCell(e);
          const detail = Math.min(e.detail, 3) as 1 | 2 | 3;
          selGranularity = detail;

          if (detail >= 2) {
            const { start, end } = applyGranularity(cell);
            selStartRef.current = start;
            selEndRef.current = end;
            selAnchorStart = start;
            selAnchorEnd = end;
            drawSelection();
          } else {
            selStartRef.current = cell;
            selEndRef.current = cell;
            selAnchorStart = null;
            selAnchorEnd = null;
          }
        }
      };

      const handleMouseMove = (e: MouseEvent) => {
        if (!e.shiftKey) {
          const t = terminalRef.current;
          if (t) {
            const mode = t.mouse_mode();
            if (mode === 3 || mode === 4) {
              const button =
                e.buttons & 1 ? 0 : e.buttons & 2 ? 2 : e.buttons & 4 ? 1 : 0;
              if (e.buttons || mode === 4) {
                sendMouseEvent("move", e, button + 32);
                return;
              }
            }
          }
        }
        if (selecting) {
          const cell = mouseToCell(e);
          if (selGranularity >= 2 && selAnchorStart && selAnchorEnd) {
            // Extend selection by word/line granularity from the anchor
            const { start: dragStart, end: dragEnd } = applyGranularity(cell);
            // Compare drag position vs anchor to determine direction
            const dragBefore =
              dragStart.row < selAnchorStart.row ||
              (dragStart.row === selAnchorStart.row && dragStart.col < selAnchorStart.col);
            if (dragBefore) {
              selStartRef.current = dragStart;
              selEndRef.current = selAnchorEnd;
            } else {
              selStartRef.current = selAnchorStart;
              selEndRef.current = dragEnd;
            }
          } else {
            selEndRef.current = cell;
          }
          drawSelection();
        }
      };

      const handleMouseUp = (e: MouseEvent) => {
        if (!e.shiftKey && sendMouseEvent("up", e, e.button)) {
          e.preventDefault();
          return;
        }
        if (selecting) {
          selecting = false;
          if (selGranularity === 1) {
            selEndRef.current = mouseToCell(e);
          }
          drawSelection();
          if (
            selStartRef.current &&
            selEndRef.current &&
            (selStartRef.current.row !== selEndRef.current.row || selStartRef.current.col !== selEndRef.current.col)
          ) {
            copySelection();
          }
          clearSelection();
        }
        if (canvas.contains(e.target as Node)) {
          inputRef.current?.focus();
        }
      };

      const handleWheel = (e: WheelEvent) => {
        const t = terminalRef.current;
        if (t && t.mouse_mode() > 0) {
          e.preventDefault();
          const button = e.deltaY < 0 ? 64 : 65;
          sendMouseEvent("down", e, button);
        } else if (ptyId !== null && status === "connected") {
          e.preventDefault();
          const lines = Math.round(-e.deltaY / 20) || (e.deltaY > 0 ? -3 : 3);
          scrollOffsetRef.current = Math.max(
            0,
            scrollOffsetRef.current + lines,
          );
          sendScroll(ptyId, scrollOffsetRef.current);
        }
      };

      // --- URL detection ---

      const URL_RE = /https?:\/\/[^\s<>"'`)\]},;]+/g;

      function urlAt(
        row: number,
        col: number,
      ): { url: string; startCol: number; endCol: number } | null {
        const text = getRowText(row);
        URL_RE.lastIndex = 0;
        let m: RegExpExecArray | null;
        while ((m = URL_RE.exec(text)) !== null) {
          const startCol = m.index;
          const raw = m[0].replace(/[.),:;]+$/, "");
          const endCol = startCol + raw.length - 1;
          if (col >= startCol && col <= endCol) {
            return { url: raw, startCol, endCol };
          }
        }
        return null;
      }

      const handleContextMenu = (e: MouseEvent) => {
        const t = terminalRef.current;
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
        inputRef.current?.focus();
      };

      let lastHoverUrl: string | null = null;
      const handleHoverMove = (e: MouseEvent) => {
        if (selecting) {
          if (hoveredUrlRef.current) {
            hoveredUrlRef.current = null;
            needsRenderRef.current = true;
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
          hoveredUrlRef.current = hit
            ? { row: cell.row, startCol: hit.startCol, endCol: hit.endCol, url: hit.url }
            : null;
          needsRenderRef.current = true;
        }
      };

      canvas.addEventListener("mousedown", handleMouseDown);
      canvas.addEventListener("mousemove", handleMouseMove);
      canvas.addEventListener("mousemove", handleHoverMove);
      window.addEventListener("mouseup", handleMouseUp);
      canvas.addEventListener("wheel", handleWheel, { passive: false });
      canvas.addEventListener("contextmenu", handleContextMenu);
      canvas.addEventListener("click", handleClick);

      return () => {
        canvas.removeEventListener("mousedown", handleMouseDown);
        canvas.removeEventListener("mousemove", handleMouseMove);
        canvas.removeEventListener("mousemove", handleHoverMove);
        window.removeEventListener("mouseup", handleMouseUp);
        canvas.removeEventListener("wheel", handleWheel);
        canvas.removeEventListener("contextmenu", handleContextMenu);
        canvas.removeEventListener("click", handleClick);
      };
    }, [ptyId, status, sendInput, sendScroll]);

    // -----------------------------------------------------------------------
    // Render
    // -----------------------------------------------------------------------

    return (
      <div
        ref={containerRef}
        className={className}
        style={{
          position: "relative",
          overflow: "hidden",
          ...style,
        }}
      >
        <canvas
          ref={glCanvasRef}
          style={
            readOnly
              ? {
                  display: "block",
                  width: "100%",
                  height: "100%",
                  objectFit: "contain",
                  objectPosition: "top left",
                  imageRendering: "pixelated",
                }
              : {
                  display: "block",
                  position: "absolute",
                  top: 0,
                  left: 0,
                  cursor: "text",
                  willChange: "transform",
                }
          }
        />
        {!readOnly && (
          <canvas
            ref={overlayCanvasRef}
            style={{
              display: "block",
              position: "absolute",
              top: 0,
              left: 0,
              pointerEvents: "none",
            }}
          />
        )}
        {!readOnly && (
          <textarea
            ref={inputRef}
            aria-label="Terminal input"
            autoCapitalize="off"
            autoComplete="off"
            autoCorrect="off"
            spellCheck={false}
            style={{
              position: "absolute",
              opacity: 0,
              width: 1,
              height: 1,
              top: 0,
              left: 0,
              padding: 0,
              border: "none",
              outline: "none",
              resize: "none",
              overflow: "hidden",
            }}
            tabIndex={0}
          />
        )}
      </div>
    );
  },
);
