import {
  useCallback,
  useEffect,
  useImperativeHandle,
  useRef,
  useState,
} from "react";
import type { Ref } from "react";
import type { Terminal } from "blit-browser";
import type { BlitTerminalProps, TerminalPalette } from "./types";
import type { ConnectionStatus } from "./types";
import { DEFAULT_FONT, DEFAULT_FONT_SIZE } from "./types";
import { MOUSE_DOWN, MOUSE_UP, MOUSE_MOVE } from "./protocol";
import {
  measureCell,
  cssFontFamily,
  type CellMetrics,
} from "./hooks/useBlitTerminal";
import { useBlitContext, useRequiredBlitWorkspace } from "./BlitContext";
import { useBlitConnection } from "./hooks/useBlitConnection";
import { useBlitSession } from "./hooks/useBlitSession";
import type { GlRenderer } from "./gl-renderer";
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
export function BlitTerminal({
  ref,
  ...props
}: BlitTerminalProps & { ref?: Ref<BlitTerminalHandle> }) {
    const ctx = useBlitContext();
    const workspace = useRequiredBlitWorkspace();
    const session = useBlitSession(props.sessionId);
    const connection = useBlitConnection(session?.connectionId);
    const blitConn = session
      ? workspace.getConnection(session.connectionId)
      : null;
    if (props.sessionId !== null && (!session || !connection || !blitConn)) {
      throw new Error(`Unknown blit session ${props.sessionId}`);
    }
    const {
      sessionId,
      fontFamily = ctx.fontFamily ?? DEFAULT_FONT,
      fontSize = ctx.fontSize ?? DEFAULT_FONT_SIZE,
      className,
      style,
      palette = ctx.palette,
      readOnly = false,
      showCursor = true,
      onRender,
      scrollbarColor = "rgba(255,255,255,0.3)",
      scrollbarWidth = 4,
      advanceRatio = ctx.advanceRatio,
    } = props;

    // Refs for DOM elements.
    const containerRef = useRef<HTMLDivElement>(null);
    const glCanvasRef = useRef<HTMLCanvasElement>(null);
    const inputRef = useRef<HTMLTextAreaElement>(null);

    // Refs for mutable state that must not trigger re-renders.
    const viewIdRef = useRef<string | null>(null);
    const terminalRef = useRef<Terminal | null>(null);
    const rendererRef = useRef<GlRenderer | null>(null);
    const displayCtxRef = useRef<CanvasRenderingContext2D | null>(null);
    const applyMetricsRef = useRef<(t: Terminal) => void>(() => {});
    const cellRef = useRef<CellMetrics>(
      measureCell(fontFamily, fontSize, undefined, advanceRatio),
    );
    const rowsRef = useRef(24);
    const colsRef = useRef(80);
    const contentDirtyRef = useRef(true);
    const lastOffsetRef = useRef(0);
    /** Track WASM buffer identity to detect heap growth that invalidates vertex pointers. */
    const lastWasmBufferRef = useRef<ArrayBuffer | null>(null);
    const [cellVersion, setCellVersion] = useState(0);
    const rafRef = useRef(0);
    const renderScheduledRef = useRef(false);

    const scrollOffsetRef = useRef(0);
    const scrollFadeRef = useRef(0);
    const scrollFadeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(
      null,
    );
    /** Scrollbar geometry in canvas pixels, updated each render. */
    const scrollbarGeoRef = useRef<{
      barX: number;
      barY: number;
      barW: number;
      barH: number;
      canvasH: number;
      totalLines: number;
      viewportRows: number;
    } | null>(null);
    const scrollDraggingRef = useRef(false);
    const scrollDragOffsetRef = useRef(0);
    const cursorBlinkOnRef = useRef(true);
    const cursorBlinkTimerRef = useRef<ReturnType<typeof setInterval> | null>(
      null,
    );
    const paletteRef = useRef<TerminalPalette | undefined>(palette);
    const showCursorRef = useRef(showCursor);
    showCursorRef.current = showCursor;

    const selStartRef = useRef<{ row: number; col: number } | null>(null);
    const selEndRef = useRef<{ row: number; col: number } | null>(null);
    const selectingRef = useRef(false);
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

    const doRenderRef = useRef(() => {});
    const scheduleRender = useCallback(() => {
      if (renderScheduledRef.current) return;
      renderScheduledRef.current = true;
      rafRef.current = requestAnimationFrame(() => {
        renderScheduledRef.current = false;
        doRenderRef.current();
      });
    }, []);

    // Re-render when showCursor (focused state) changes.
    useEffect(() => {
      contentDirtyRef.current = true;
      scheduleRender();
    }, [showCursor, scheduleRender]);

    // -----------------------------------------------------------------------
    // Connection callbacks — BlitTerminal only cares about UPDATE (rendering)
    // -----------------------------------------------------------------------

    const status: ConnectionStatus = connection?.status ?? "disconnected";

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
      contentDirtyRef.current = true;
      scheduleRender();
    }, []);

    useEffect(() => {
      if (!blitConn || sessionId === null) return;
      const syncReadOnlySize = (t: Terminal) => {
        const tr = t.rows;
        const tc = t.cols;
        if (tr !== rowsRef.current || tc !== colsRef.current) {
          rowsRef.current = tr;
          colsRef.current = tc;
        }
        scheduleRender();
      };
      const unsub = blitConn.addDirtyListener(sessionId, () => {
        const t = blitConn.getTerminal(sessionId);
        if (!t) return;
        if (terminalRef.current !== t) {
          terminalRef.current = t;
          applyPaletteToTerminal(t);
          applyMetricsRef.current(t);
        }
        contentDirtyRef.current = true;
        scheduleRender();
        reconcilePrediction();
        if (readOnly) syncReadOnlySize(t);
      });
      // Check for a terminal that was created between render and this effect.
      if (sessionId !== null && blitConn) {
        const t = blitConn.getTerminal(sessionId);
        if (t) {
          if (terminalRef.current !== t) {
            terminalRef.current = t;
            applyPaletteToTerminal(t);
            applyMetricsRef.current(t);
          }
          contentDirtyRef.current = true;
          scheduleRender();
          if (readOnly) syncReadOnlySize(t);
        }
      }
      return () => {
        unsub();
      };
    }, [
      sessionId,
      readOnly,
      blitConn,
      reconcilePrediction,
      applyPaletteToTerminal,
    ]);
    const sendInput = useCallback(
      (id: string, data: Uint8Array) => {
        workspace.sendInput(id, data);
      },
      [workspace],
    );

    const sendScroll = useCallback(
      (id: string, offset: number) => {
        workspace.scrollSession(id, offset);
      },
      [workspace],
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
      if (!blitConn) {
        setWasmReady(false);
        return;
      }
      const unsub = blitConn.onReady(() => setWasmReady(true));
      if (blitConn.isReady()) setWasmReady(true);
      return unsub;
    }, [blitConn]);

    // -----------------------------------------------------------------------
    // Cell measurement (re-measure when font or DPR changes)
    // -----------------------------------------------------------------------

    // Chrome and Firefox update devicePixelRatio on zoom; Safari doesn't.
    // Detect zoom via outerWidth/innerWidth on Safari only — on other browsers
    // outerWidth includes browser chrome, making the ratio unreliable.
    const isSafari = /^((?!chrome|android).)*safari/i.test(navigator.userAgent);

    function effectiveDpr(): number {
      const base = window.devicePixelRatio || 1;
      if (isSafari && window.outerWidth && window.innerWidth) {
        const zoom = window.outerWidth / window.innerWidth;
        if (zoom > 0.25 && zoom < 8) return Math.round(base * zoom * 100) / 100;
      }
      return base;
    }

    const [dpr, setDpr] = useState(effectiveDpr);
    const dprRef = useRef(dpr);
    dprRef.current = dpr;
    applyMetricsRef.current = (t: Terminal) => {
      const cell = cellRef.current;
      t.set_cell_size(cell.pw, cell.ph);
      t.set_font_family(fontFamily);
      t.set_font_size(fontSize * dpr);
      t.invalidate_render_cache();
    };

    useEffect(() => {
      const check = () => {
        const next = effectiveDpr();
        if (next !== dprRef.current) setDpr(next);
      };
      let mq: MediaQueryList | null = null;
      if (typeof window.matchMedia === "function") {
        mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
        mq.addEventListener("change", check);
      }
      window.addEventListener("resize", check);
      return () => {
        mq?.removeEventListener("change", check);
        window.removeEventListener("resize", check);
      };
    }, []);

    useEffect(() => {
      if (!blitConn) return;
      const rasterFontSize = fontSize * dpr;
      const apply = (forceInvalidate = false) => {
        const cell = measureCell(fontFamily, fontSize, dpr, advanceRatio);
        const changed =
          cell.pw !== cellRef.current.pw || cell.ph !== cellRef.current.ph;
        const shouldInvalidate = forceInvalidate || changed;
        cellRef.current = cell;
        if (!readOnly) {
          const t = terminalRef.current;
          if (t) {
            t.set_cell_size(cell.pw, cell.ph);
            t.set_font_family(fontFamily);
            t.set_font_size(rasterFontSize);
            if (shouldInvalidate) t.invalidate_render_cache();
          }
          blitConn.setCellSize(cell.pw, cell.ph);
          blitConn.setFontFamily(fontFamily);
          blitConn.setFontSize(rasterFontSize);
        }
        if (shouldInvalidate) {
          contentDirtyRef.current = true;
          scheduleRender();
        }
        if (changed) {
          setCellVersion((v) => v + 1);
        }
      };
      // Always apply on effect run (font/size/dpr changed).
      contentDirtyRef.current = true;
      apply(true);
      scheduleRender();
      // Re-measure when fonts finish loading. Even if metrics stay the same,
      // the glyph atlas may need a rebuild because the actual raster changed.
      const onFontsLoaded = () => apply(true);
      document.fonts?.addEventListener("loadingdone", onFontsLoaded);
      // If fonts already loaded while we were setting up, re-apply now.
      if (document.fonts?.status === "loaded") apply(true);
      return () =>
        document.fonts?.removeEventListener("loadingdone", onFontsLoaded);
    }, [
      fontFamily,
      fontSize,
      dpr,
      blitConn,
      readOnly,
      scheduleRender,
      advanceRatio,
    ]);

    // -----------------------------------------------------------------------
    // Cursor blink timer
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (readOnly) return;
      cursorBlinkOnRef.current = true;
      const timer = setInterval(() => {
        cursorBlinkOnRef.current = !cursorBlinkOnRef.current;
        scheduleRender();
      }, 530);
      cursorBlinkTimerRef.current = timer;
      return () => {
        clearInterval(timer);
        cursorBlinkTimerRef.current = null;
      };
    }, [readOnly, scheduleRender]);

    // -----------------------------------------------------------------------
    // GL renderer lifecycle
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (!blitConn) return;
      const shared = blitConn.getSharedRenderer();
      if (shared) rendererRef.current = shared.renderer;
    }, [blitConn]);

    // -----------------------------------------------------------------------
    // Terminal instance lifecycle
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (!blitConn) {
        terminalRef.current = null;
        return;
      }
      if (sessionId !== null) {
        blitConn.retain(sessionId);
        const t = blitConn.getTerminal(sessionId);
        if (t) {
          terminalRef.current = t;
          applyPaletteToTerminal(t);
          if (!readOnly) {
            const cell = cellRef.current;
            t.set_cell_size(cell.pw, cell.ph);
            t.set_font_family(fontFamily);
            t.set_font_size(fontSize * dpr);
          }
          contentDirtyRef.current = true;
          scheduleRender();
        }
      } else {
        terminalRef.current = null;
      }
      return () => {
        terminalRef.current = null;
        if (sessionId !== null && blitConn) blitConn.release(sessionId);
      };
    }, [
      wasmReady,
      sessionId,
      blitConn,
      fontFamily,
      fontSize,
      readOnly,
      applyPaletteToTerminal,
    ]);

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

      const handleResize = () => {
        const cell = cellRef.current;
        const w = container.clientWidth;
        const h = container.clientHeight;
        const cols = Math.max(1, Math.floor(w / cell.w));
        const rows = Math.max(1, Math.floor(h / cell.h));

        const sizeChanged =
          cols !== colsRef.current || rows !== rowsRef.current;
        if (sizeChanged) {
          rowsRef.current = rows;
          colsRef.current = cols;
          if (sessionId !== null && blitConn && viewIdRef.current) {
            blitConn.setViewSize(sessionId, viewIdRef.current, rows, cols);
          }
        }
        // Always schedule — DPR may have changed even if cols/rows didn't.
        contentDirtyRef.current = true;
        scheduleRender();
      };

      // Allocate a view ID for multi-pane size tracking.
      if (!viewIdRef.current && blitConn) {
        viewIdRef.current = blitConn.allocViewId();
      }

      const observer = new ResizeObserver(handleResize);
      observer.observe(container);
      window.addEventListener("resize", handleResize);
      handleResize();

      return () => {
        observer.disconnect();
        window.removeEventListener("resize", handleResize);
        if (sessionId !== null && blitConn && viewIdRef.current) {
          blitConn.removeView(sessionId, viewIdRef.current);
        }
      };
    }, [sessionId, readOnly, blitConn, fontFamily, fontSize, dpr, cellVersion]);

    // Re-send dimensions when the connection becomes ready.
    useEffect(() => {
      if (
        status === "connected" &&
        sessionId !== null &&
        !readOnly &&
        blitConn &&
        viewIdRef.current
      ) {
        const rows = rowsRef.current;
        const cols = colsRef.current;
        if (rows > 0 && cols > 0) {
          blitConn.setViewSize(sessionId, viewIdRef.current, rows, cols);
        }
      }
    }, [status, sessionId, readOnly, blitConn]);

    // -----------------------------------------------------------------------
    // Render (demand-driven — only rAF when something changed)
    // -----------------------------------------------------------------------

    useEffect(() => {
      if (!blitConn) return;
      doRenderRef.current = () => {
        const t0 = performance.now();
        if (!rendererRef.current?.supported) {
          const shared = blitConn.getSharedRenderer();
          if (shared) rendererRef.current = shared.renderer;
          if (!rendererRef.current?.supported) {
            if (!readOnly) blitConn.noteFrameRendered();
            return;
          }
        }
        if (!terminalRef.current) {
          if (!readOnly) blitConn.noteFrameRendered();
          return;
        }

        {
          const t = terminalRef.current;
          const cell = cellRef.current;
          const renderer = rendererRef.current;

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
          }

          const mem = blitConn.wasmMemory()!;
          // Detect WASM heap growth: if the underlying ArrayBuffer changed,
          // vertex pointers from the previous prepare_render_ops are invalid.
          if (mem.buffer !== lastWasmBufferRef.current) {
            lastWasmBufferRef.current = mem.buffer;
            contentDirtyRef.current = true;
          }

          {
            const gridH = t.rows * cell.ph;
            const gridW = t.cols * cell.pw;
            const xOff = Math.max(0, Math.floor((pw - gridW) / 2));
            const yOff = Math.max(0, Math.floor((ph - gridH) / 2));
            const combined = xOff * 65536 + yOff;
            if (combined !== lastOffsetRef.current) {
              lastOffsetRef.current = combined;
              t.set_render_offset(xOff, yOff);
              contentDirtyRef.current = true;
            }
          }

          if (contentDirtyRef.current) {
            contentDirtyRef.current = false;
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
          renderer.render(
            bgVerts,
            glyphVerts,
            t.glyph_atlas_canvas(),
            t.glyph_atlas_version(),
            t.cursor_visible(),
            t.cursor_col,
            t.cursor_row,
            t.cursor_style(),
            cursorBlinkOnRef.current,
            cell,
            paletteRef.current?.bg ?? [0, 0, 0],
            showCursorRef.current,
          );

          // Copy GL to display canvas, then draw overlay content on top.
          const shared = blitConn.getSharedRenderer();
          const displayCanvas = glCanvasRef.current;
          if (shared && displayCanvas) {
            if (displayCanvas.width !== pw) {
              displayCanvas.width = pw;
              displayCtxRef.current = null;
            }
            if (displayCanvas.height !== ph) {
              displayCanvas.height = ph;
              displayCtxRef.current = null;
            }
            if (!displayCtxRef.current) {
              displayCtxRef.current = displayCanvas.getContext("2d");
              // Disable any implicit DPR scaling — we manage pixels ourselves.
              displayCtxRef.current?.resetTransform();
            }
            const ctx = displayCtxRef.current;
            if (ctx) {
              ctx.drawImage(shared.canvas, 0, 0, pw, ph, 0, 0, pw, ph);

              // Selection highlight
              const ss = selStartRef.current;
              const se = selEndRef.current;
              if (ss && se) {
                let sr = ss.row,
                  sc = ss.col;
                let er = se.row,
                  ec = se.col;
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
                const [fgR, fgG, fgB] = paletteRef.current?.fg ?? [
                  204, 204, 204,
                ];
                ctx.strokeStyle = `rgba(${fgR},${fgG},${fgB},0.6)`;
                ctx.lineWidth = Math.max(1, Math.round(cell.ph * 0.06));
                const y = hurl.row * cell.ph + cell.ph - ctx.lineWidth;
                ctx.beginPath();
                ctx.moveTo(hurl.startCol * cell.pw, y);
                ctx.lineTo((hurl.endCol + 1) * cell.pw, y);
                ctx.stroke();
              }

              // Overflow text (emoji / wide Unicode)
              const overflowCount = t.overflow_text_count();
              if (overflowCount > 0) {
                const cw = cell.pw;
                const ch = cell.ph;
                const scale = 0.85;
                const scaledH = ch * scale;
                const fSize = Math.max(1, Math.round(scaledH));
                ctx.font = `${fSize}px ${cssFontFamily(fontFamily)}`;
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

              // Predicted echo
              if (!readOnly && predictedRef.current) {
                const t2 = terminalRef.current;
                if (t2 && t2.echo()) {
                  const cw = cell.pw;
                  const ch = cell.ph;
                  const [fR, fG, fB] = paletteRef.current?.fg ?? [
                    204, 204, 204,
                  ];
                  ctx.fillStyle = `rgba(${fR},${fG},${fB},0.5)`;
                  const fSize = Math.max(1, Math.round(ch * 0.85));
                  ctx.font = `${fSize}px ${cssFontFamily(fontFamily)}`;
                  ctx.textBaseline = "bottom";
                  const pred = predictedRef.current;
                  const cc = t2.cursor_col;
                  const cr = t2.cursor_row;
                  for (
                    let i = 0;
                    i < pred.length && cc + i < colsRef.current;
                    i++
                  ) {
                    ctx.fillText(pred[i], (cc + i) * cw, cr * ch + ch);
                  }
                }
              }

              // Scrollbar — always compute geometry for hit testing
              {
                const t2 = terminalRef.current;
                if (t2) {
                  const totalLines = t2.scrollback_lines() + rowsRef.current;
                  const viewportRows = rowsRef.current;
                  if (totalLines > viewportRows) {
                    const ch = cell.ph;
                    const canvasH = viewportRows * ch;
                    const barW = scrollbarWidth;
                    const barH = Math.max(
                      barW,
                      (viewportRows / totalLines) * canvasH,
                    );
                    const maxScroll = totalLines - viewportRows;
                    const scrollFraction =
                      Math.min(scrollOffsetRef.current, maxScroll) / maxScroll;
                    const barY = (1 - scrollFraction) * (canvasH - barH);
                    const barX = colsRef.current * cell.pw - barW - 2;
                    scrollbarGeoRef.current = {
                      barX,
                      barY,
                      barW,
                      barH,
                      canvasH,
                      totalLines,
                      viewportRows,
                    };
                    const show =
                      scrollFadeRef.current > 0 ||
                      scrollDraggingRef.current ||
                      scrollOffsetRef.current > 0;
                    if (show) {
                      ctx.fillStyle = scrollbarColor;
                      ctx.beginPath();
                      ctx.roundRect(barX, barY, barW, barH, barW / 2);
                      ctx.fill();
                    }
                  } else {
                    scrollbarGeoRef.current = null;
                  }
                }
              }
            }
          }

          if (!readOnly) {
            blitConn.noteFrameRendered();
          }
          onRender?.(performance.now() - t0);
        }
      };

      // Initial render.
      scheduleRender();

      return () => {
        cancelAnimationFrame(rafRef.current);
        renderScheduledRef.current = false;
      };
    }, [fontFamily, fontSize, dpr, readOnly, scheduleRender, blitConn]);

    // -----------------------------------------------------------------------
    // Keyboard input
    // -----------------------------------------------------------------------

    useEffect(() => {
      const input = inputRef.current;
      if (!input || readOnly) return;

      const handleKeyDown = (e: KeyboardEvent) => {
        if (e.defaultPrevented) return;
        if (sessionId === null || status !== "connected") return;
        if (e.isComposing) return;
        if (e.key === "Dead") return;

        // Shift+PageUp/PageDown: scroll the scrollback
        if (e.shiftKey && (e.key === "PageUp" || e.key === "PageDown")) {
          const t2 = terminalRef.current;
          const maxScroll = t2 ? t2.scrollback_lines() : 0;
          if (maxScroll > 0 || scrollOffsetRef.current > 0) {
            e.preventDefault();
            const delta =
              e.key === "PageUp" ? rowsRef.current : -rowsRef.current;
            scrollOffsetRef.current = Math.max(
              0,
              Math.min(maxScroll, scrollOffsetRef.current + delta),
            );
            sendScroll(sessionId, scrollOffsetRef.current);
            scrollFadeRef.current = 1;
            if (scrollFadeTimerRef.current)
              clearTimeout(scrollFadeTimerRef.current);
            scrollFadeTimerRef.current = setTimeout(() => {
              scrollFadeRef.current = 0;
              scheduleRender();
            }, 1000);
            scheduleRender();
          }
          return;
        }
        // Shift+Home/End: jump to top/bottom of scrollback
        if (e.shiftKey && (e.key === "Home" || e.key === "End")) {
          const t2 = terminalRef.current;
          const maxScroll = t2 ? t2.scrollback_lines() : 0;
          if (maxScroll > 0 || scrollOffsetRef.current > 0) {
            e.preventDefault();
            scrollOffsetRef.current = e.key === "Home" ? maxScroll : 0;
            sendScroll(sessionId, scrollOffsetRef.current);
            scrollFadeRef.current = 1;
            if (scrollFadeTimerRef.current)
              clearTimeout(scrollFadeTimerRef.current);
            scrollFadeTimerRef.current = setTimeout(() => {
              scrollFadeRef.current = 0;
              scheduleRender();
            }, 1000);
            scheduleRender();
          }
          return;
        }

        const t = terminalRef.current;
        const appCursor = t ? t.app_cursor() : false;
        const bytes = keyToBytes(e, appCursor);
        if (bytes) {
          e.preventDefault();
          if (scrollOffsetRef.current > 0) {
            scrollOffsetRef.current = 0;
            sendScroll(sessionId, 0);
          }
          if (
            t &&
            t.echo() &&
            e.key.length === 1 &&
            !e.ctrlKey &&
            !e.metaKey &&
            !e.altKey
          ) {
            if (!predictedRef.current) {
              predictedFromRowRef.current = t.cursor_row;
              predictedFromColRef.current = t.cursor_col;
            }
            predictedRef.current += e.key;
            scheduleRender();
          } else {
            predictedRef.current = "";
          }
          sendInput(sessionId, bytes);
        }
      };

      const handleCompositionEnd = (e: CompositionEvent) => {
        if (e.data && sessionId !== null && status === "connected") {
          sendInput(sessionId, encoder.encode(e.data));
        }
        input.value = "";
      };

      const handleInput = (e: Event) => {
        const inputEvent = e as InputEvent;
        if (inputEvent.isComposing) {
          if (
            inputEvent.inputType === "deleteContentBackward" &&
            !input.value &&
            sessionId !== null &&
            status === "connected"
          ) {
            sendInput(sessionId, new Uint8Array([0x7f]));
          }
          return;
        }
        if (inputEvent.inputType === "deleteContentBackward" && !input.value) {
          if (sessionId !== null && status === "connected") {
            sendInput(sessionId, new Uint8Array([0x7f]));
          }
        } else if (
          input.value &&
          sessionId !== null &&
          status === "connected"
        ) {
          sendInput(
            sessionId,
            encoder.encode(input.value.replace(/\n/g, "\r")),
          );
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
    }, [sessionId, status, readOnly, sendInput, sendScroll]);

    // -----------------------------------------------------------------------
    // Wheel scroll (works on unfocused panes too)
    // -----------------------------------------------------------------------

    useEffect(() => {
      const container = containerRef.current;
      if (!container || readOnly) return;

      const handleWheel = (e: WheelEvent) => {
        const t = terminalRef.current;
        if (t && t.mouse_mode() > 0 && !e.shiftKey) {
          // Mouse mode: handled by the mouse input effect below.
          return;
        }
        if (sessionId !== null && status === "connected") {
          const maxScroll = t ? t.scrollback_lines() : 0;
          if (maxScroll === 0 && scrollOffsetRef.current === 0) return;
          e.preventDefault();
          const delta =
            Math.abs(e.deltaY) > Math.abs(e.deltaX) ? e.deltaY : e.deltaX;
          const lines = Math.round(-delta / 20) || (delta > 0 ? -3 : 3);
          scrollOffsetRef.current = Math.max(
            0,
            Math.min(maxScroll, scrollOffsetRef.current + lines),
          );
          sendScroll(sessionId, scrollOffsetRef.current);
          if (scrollOffsetRef.current > 0) {
            scrollFadeRef.current = 1;
            if (scrollFadeTimerRef.current)
              clearTimeout(scrollFadeTimerRef.current);
            scrollFadeTimerRef.current = setTimeout(() => {
              scrollFadeRef.current = 0;
              scheduleRender();
            }, 1000);
          }
          scheduleRender();
        }
      };

      container.addEventListener("wheel", handleWheel, { passive: false });
      return () => container.removeEventListener("wheel", handleWheel);
    }, [sessionId, status, readOnly, sendScroll, scheduleRender]);

    // -----------------------------------------------------------------------
    // Mouse input
    // -----------------------------------------------------------------------

    useEffect(() => {
      const canvas = glCanvasRef.current;
      if (!canvas || readOnly || !blitConn) return;

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

      // --- Scrollbar interaction helpers ---
      const SCROLLBAR_HIT_PX = 20; // CSS px hit zone from right edge

      function canvasYFromEvent(e: MouseEvent): number {
        const rect = canvas!.getBoundingClientRect();
        const dpr = cellRef.current.pw / cellRef.current.w;
        return (e.clientY - rect.top) * dpr;
      }

      function isNearScrollbar(e: MouseEvent): boolean {
        const rect = canvas!.getBoundingClientRect();
        return e.clientX >= rect.right - SCROLLBAR_HIT_PX;
      }

      function scrollToCanvasY(y: number) {
        const geo = scrollbarGeoRef.current;
        if (!geo || sessionId === null || status !== "connected") return;
        const fraction = 1 - y / (geo.canvasH - geo.barH);
        const maxScroll = geo.totalLines - geo.viewportRows;
        const offset = Math.round(
          Math.max(0, Math.min(maxScroll, fraction * maxScroll)),
        );
        scrollOffsetRef.current = offset;
        sendScroll(sessionId, offset);
        scrollFadeRef.current = 1;
        scheduleRender();
      }

      /** Send a structured mouse event to the server (C2S_MOUSE).
       *  The server generates the correct escape sequence based on
       *  the terminal's current mouse mode, encoding, and cooked-mode state. */
      function sendMouseEvent(
        type: "down" | "up" | "move",
        e: MouseEvent,
        button: number,
      ): boolean {
        if (sessionId === null || status !== "connected") return false;
        // Quick client-side check to avoid unnecessary messages.
        // The server does the authoritative check.
        const t = terminalRef.current;
        if (t && t.mouse_mode() === 0) return false;

        const pos = mouseToCell(e);
        const typeCode =
          type === "down" ? MOUSE_DOWN : type === "up" ? MOUSE_UP : MOUSE_MOVE;
        workspace.sendMouse(sessionId, typeCode, button, pos.col, pos.row);
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
        scheduleRender();
      }

      function drawSelection() {
        scheduleRender();
      }

      function getRowText(row: number): string {
        const t = terminalRef.current;
        if (!t) return "";
        return t.get_text(row, 0, row, colsRef.current - 1);
      }

      const WORD_CHARS = /[A-Za-z0-9_\-./~:@]/;

      function wordBoundsAt(
        row: number,
        col: number,
      ): { start: number; end: number } {
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
        const t = terminalRef.current;
        return t ? t.is_wrapped(row) : false;
      }

      function logicalLineRange(row: number): {
        startRow: number;
        endRow: number;
      } {
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
        let sr = selStartRef.current.row,
          sc = selStartRef.current.col;
        let er = selEndRef.current.row,
          ec = selEndRef.current.col;
        if (sr > er || (sr === er && sc > ec)) {
          [sr, sc, er, ec] = [er, ec, sr, sc];
        }
        const text = t.get_text(sr, sc, er, ec);
        if (text) {
          navigator.clipboard.writeText(text);
        }
      }

      let mouseDownButton = -1;
      let lastMouseCell = { row: -1, col: -1 };
      const handleMouseDown = (e: MouseEvent) => {
        // Scrollbar click/drag
        if (e.button === 0 && scrollbarGeoRef.current && isNearScrollbar(e)) {
          e.preventDefault();
          const geo = scrollbarGeoRef.current;
          const y = canvasYFromEvent(e);
          scrollDraggingRef.current = true;
          canvas.style.cursor = "grabbing";
          if (y >= geo.barY && y <= geo.barY + geo.barH) {
            // Clicked on thumb — anchor for relative drag
            scrollDragOffsetRef.current = y - geo.barY;
          } else {
            // Clicked on track — jump thumb center to click
            scrollDragOffsetRef.current = geo.barH / 2;
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
          selectingRef.current = true;
          // Don't freeze yet — freeze on first drag movement so clicks
          // don't pause video players (mpv -vo tct, etc.)
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
        if (scrollDraggingRef.current) {
          scrollToCanvasY(canvasYFromEvent(e) - scrollDragOffsetRef.current);
          return;
        }
        // Only forward mouse events when a button is held (drag in progress)
        // or the cursor is actually over the terminal canvas.
        const overCanvas =
          mouseDownButton >= 0 || canvas.contains(e.target as Node);
        if (!e.shiftKey && overCanvas) {
          const t = terminalRef.current;
          if (t) {
            const mode = t.mouse_mode();
            if (mode >= 3) {
              // Only report when the cell coordinate changes (like real terminals).
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
          // Freeze on first drag so selection text is stable, but not
          // on mousedown alone (which would pause video players).
          if (sessionId !== null && blitConn && !blitConn.isFrozen(sessionId))
            blitConn.freeze(sessionId);
          const cell = mouseToCell(e);
          if (selGranularity >= 2 && selAnchorStart && selAnchorEnd) {
            // Extend selection by word/line granularity from the anchor
            const { start: dragStart, end: dragEnd } = applyGranularity(cell);
            // Compare drag position vs anchor to determine direction
            const dragBefore =
              dragStart.row < selAnchorStart.row ||
              (dragStart.row === selAnchorStart.row &&
                dragStart.col < selAnchorStart.col);
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
        if (scrollDraggingRef.current) {
          scrollDraggingRef.current = false;
          canvas.style.cursor = "text";
          scheduleRender();
          return;
        }
        if (mouseDownButton >= 0) {
          sendMouseEvent("up", e, mouseDownButton);
          mouseDownButton = -1;
          return;
        }
        if (selecting) {
          selecting = false;
          selectingRef.current = false;
          if (selGranularity === 1) {
            selEndRef.current = mouseToCell(e);
          }
          drawSelection();
          if (
            selStartRef.current &&
            selEndRef.current &&
            (selStartRef.current.row !== selEndRef.current.row ||
              selStartRef.current.col !== selEndRef.current.col)
          ) {
            copySelection();
          }
          clearSelection();
          if (sessionId !== null && blitConn) blitConn.thaw(sessionId);
        }
        if (canvas.contains(e.target as Node)) {
          inputRef.current?.focus();
        }
      };

      const handleWheel = (e: WheelEvent) => {
        const t = terminalRef.current;
        // Mouse-mode wheel: forward to the application.
        if (t && t.mouse_mode() > 0 && !e.shiftKey) {
          e.preventDefault();
          const button = e.deltaY < 0 ? 64 : 65;
          sendMouseEvent("down", e, button);
        }
        // Scrollback is handled by the container-level wheel handler above.
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
        // Scrollbar cursor
        if (scrollDraggingRef.current) {
          canvas.style.cursor = "grabbing";
          return;
        }
        if (scrollbarGeoRef.current && isNearScrollbar(e)) {
          canvas.style.cursor = "default";
          return;
        }

        if (selecting) {
          if (hoveredUrlRef.current) {
            hoveredUrlRef.current = null;
            scheduleRender();
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
            ? {
                row: cell.row,
                startCol: hit.startCol,
                endCol: hit.endCol,
                url: hit.url,
              }
            : null;
          scheduleRender();
        }
      };

      // Send a synthetic mouseup when the window loses focus, so apps
      // like zellij/tmux don't get stuck in a "button held" state.
      const handleBlur = () => {
        if (mouseDownButton >= 0) {
          // Synthetic release at (0,0) — server handles encoding + mode check
          if (sessionId !== null && status === "connected") {
            workspace.sendMouse(sessionId, MOUSE_UP, mouseDownButton, 0, 0);
          }
          mouseDownButton = -1;
        }
        if (selecting) {
          selecting = false;
          selectingRef.current = false;
          clearSelection();
          if (sessionId !== null && blitConn) blitConn.thaw(sessionId);
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

      return () => {
        canvas.removeEventListener("mousedown", handleMouseDown);
        window.removeEventListener("mousemove", handleMouseMove);
        canvas.removeEventListener("mousemove", handleHoverMove);
        window.removeEventListener("mouseup", handleMouseUp);
        window.removeEventListener("blur", handleBlur);
        canvas.removeEventListener("wheel", handleWheel);
        canvas.removeEventListener("contextmenu", handleContextMenu);
        canvas.removeEventListener("click", handleClick);
        if (scrollFadeTimerRef.current)
          clearTimeout(scrollFadeTimerRef.current);
      };
    }, [sessionId, status, sendScroll, blitConn, workspace, readOnly]);

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
                  objectPosition: "center",
                }
              : {
                  display: "block",
                  position: "absolute",
                  top: 0,
                  left: 0,
                  cursor: "text",
                }
          }
        />
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
}
