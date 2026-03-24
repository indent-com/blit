import { useCallback, useEffect, useRef, useState } from 'react';
import type { Terminal } from 'blit-browser';

const DEFAULT_FONT = '"PragmataPro Liga", "PragmataPro", ui-monospace, monospace';

export interface CellMetrics {
  /** CSS pixel width. */
  w: number;
  /** CSS pixel height. */
  h: number;
  /** Device pixel width (integer). */
  pw: number;
  /** Device pixel height (integer). */
  ph: number;
}

/**
 * Measure the dimensions of a single monospace cell by rendering 'M'
 * into a hidden span, then snapping to device pixel boundaries.
 */
export function measureCell(fontFamily: string, fontSize: number): CellMetrics {
  const span = document.createElement('span');
  span.style.cssText = `font: ${fontSize}px ${fontFamily}; position: absolute; visibility: hidden; white-space: pre;`;
  span.textContent = 'M';
  document.body.appendChild(span);
  const rect = span.getBoundingClientRect();
  document.body.removeChild(span);

  const dpr = window.devicePixelRatio || 1;
  const pw = Math.round(rect.width * dpr);
  const ph = Math.round(rect.height * dpr);
  return { w: pw / dpr, h: ph / dpr, pw, ph };
}

export interface UseBlitTerminalOptions {
  fontFamily?: string;
  fontSize?: number;
  /**
   * When provided, skips the DOM measurement probe and uses these metrics
   * directly. Useful when the consumer already knows cell dimensions
   * from a shared font configuration.
   */
  initialCellMetrics?: CellMetrics;
}

/**
 * Hook managing a WASM Terminal instance, cell measurement, and
 * grid dimension computation for a given container size.
 */
export function useBlitTerminal(options?: UseBlitTerminalOptions) {
  const fontFamily = options?.fontFamily ?? DEFAULT_FONT;
  const fontSize = options?.fontSize ?? 13;

  const [cell, setCell] = useState<CellMetrics>(
    () => options?.initialCellMetrics ?? measureCell(fontFamily, fontSize),
  );
  const terminalRef = useRef<Terminal | null>(null);
  const [dimensions, setDimensions] = useState<{ rows: number; cols: number }>({
    rows: 24,
    cols: 80,
  });

  useEffect(() => {
    if (options?.initialCellMetrics) {
      setCell(options.initialCellMetrics);
      if (terminalRef.current) {
        terminalRef.current.set_cell_size(options.initialCellMetrics.pw, options.initialCellMetrics.ph);
        terminalRef.current.set_font_family(fontFamily);
      }
      return;
    }
    const newCell = measureCell(fontFamily, fontSize);
    setCell(newCell);
    if (terminalRef.current) {
      terminalRef.current.set_cell_size(newCell.pw, newCell.ph);
      terminalRef.current.set_font_family(fontFamily);
    }
  }, [fontFamily, fontSize, options?.initialCellMetrics]);

  const computeSize = useCallback(
    (containerWidth: number, containerHeight: number) => {
      const cols = Math.max(1, Math.floor(containerWidth / cell.w));
      const rows = Math.max(1, Math.floor(containerHeight / cell.h));
      setDimensions({ rows, cols });
      return { rows, cols };
    },
    [cell],
  );

  const createTerminal = useCallback(
    (TerminalClass: typeof Terminal, rows: number, cols: number) => {
      if (terminalRef.current) {
        terminalRef.current.free();
      }
      const t = new TerminalClass(rows, cols, cell.pw, cell.ph);
      t.set_font_family(fontFamily);
      terminalRef.current = t;
      return t;
    },
    [cell, fontFamily],
  );

  const destroyTerminal = useCallback(() => {
    if (terminalRef.current) {
      terminalRef.current.free();
      terminalRef.current = null;
    }
  }, []);

  return {
    cell,
    dimensions,
    terminal: terminalRef.current,
    terminalRef,
    computeSize,
    createTerminal,
    destroyTerminal,
  };
}
