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
import {
  C2S_INPUT,
  C2S_RESIZE,
  C2S_SCROLL,
  DEFAULT_FONT,
  DEFAULT_FONT_SIZE,
} from "./types";
import { measureCell, type CellMetrics } from "./hooks/useBlitTerminal";
const BG_OP_STRIDE = 4;
const GLYPH_OP_STRIDE = 8;
const MAX_BATCH_VERTS = 65532;

// ---------------------------------------------------------------------------
// Shader sources (ported verbatim from web/index.html)
// ---------------------------------------------------------------------------

const RECT_VS = `
attribute vec2 a_pos;
attribute vec4 a_color;
uniform vec2 u_resolution;
varying vec4 v_color;

void main() {
    vec2 zeroToOne = a_pos / u_resolution;
    vec2 zeroToTwo = zeroToOne * 2.0;
    vec2 clip = zeroToTwo - 1.0;
    gl_Position = vec4(clip * vec2(1.0, -1.0), 0.0, 1.0);
    v_color = a_color;
}
`;

const RECT_FS = `
precision mediump float;
varying vec4 v_color;

void main() {
    gl_FragColor = vec4(v_color.rgb * v_color.a, v_color.a);
}
`;

const GLYPH_VS = `
attribute vec2 a_pos;
attribute vec2 a_uv;
attribute vec4 a_color;
uniform vec2 u_resolution;
varying vec2 v_uv;
varying vec4 v_color;

void main() {
    vec2 zeroToOne = a_pos / u_resolution;
    vec2 zeroToTwo = zeroToOne * 2.0;
    vec2 clip = zeroToTwo - 1.0;
    gl_Position = vec4(clip * vec2(1.0, -1.0), 0.0, 1.0);
    v_uv = a_uv;
    v_color = a_color;
}
`;

const GLYPH_FS = `
precision mediump float;
varying vec2 v_uv;
varying vec4 v_color;
uniform sampler2D u_texture;

void main() {
    vec4 tex = texture2D(u_texture, v_uv);
    float minC = min(tex.r, min(tex.g, tex.b));
    float maxC = max(tex.r, max(tex.g, tex.b));
    float isGray = step(maxC - minC, 0.02);
    vec3 tinted = v_color.rgb * tex.a;
    gl_FragColor = vec4(mix(tex.rgb, tinted, isGray), tex.a);
}
`;

// ---------------------------------------------------------------------------
// GL helpers
// ---------------------------------------------------------------------------

function compileShader(
  gl: WebGLRenderingContext,
  type: number,
  source: string,
): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (gl.getShaderParameter(shader, gl.COMPILE_STATUS)) return shader;
  gl.deleteShader(shader);
  return null;
}

function createProgram(
  gl: WebGLRenderingContext,
  vs: string,
  fs: string,
): WebGLProgram | null {
  const vertexShader = compileShader(gl, gl.VERTEX_SHADER, vs);
  const fragmentShader = compileShader(gl, gl.FRAGMENT_SHADER, fs);
  if (!vertexShader || !fragmentShader) return null;
  const program = gl.createProgram();
  if (!program) return null;
  gl.attachShader(program, vertexShader);
  gl.attachShader(program, fragmentShader);
  gl.linkProgram(program);
  gl.deleteShader(vertexShader);
  gl.deleteShader(fragmentShader);
  if (gl.getProgramParameter(program, gl.LINK_STATUS)) return program;
  gl.deleteProgram(program);
  return null;
}

// ---------------------------------------------------------------------------
// GL renderer
// ---------------------------------------------------------------------------

interface GlRenderer {
  supported: boolean;
  resize(width: number, height: number): void;
  render(
    bgOps: Uint32Array,
    glyphOps: Uint32Array,
    atlasCanvas: HTMLCanvasElement | undefined,
    atlasVersion: number,
    cursorVisible: boolean,
    cursorCol: number,
    cursorRow: number,
    cursorStyle: number,
    cursorBlinkOn: boolean,
    cell: CellMetrics,
    bgColor: [number, number, number],
  ): void;
  dispose(): void;
}

function createGlRenderer(canvas: HTMLCanvasElement): GlRenderer {
  const gl = canvas.getContext("webgl", {
    alpha: true,
    antialias: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: true,
  });

  if (!gl) {
    return {
      supported: false,
      resize() {},
      render() {},
      dispose() {},
    };
  }

  const rectProgram = createProgram(gl, RECT_VS, RECT_FS);
  const glyphProgram = createProgram(gl, GLYPH_VS, GLYPH_FS);

  if (!rectProgram || !glyphProgram) {
    return {
      supported: false,
      resize() {},
      render() {},
      dispose() {},
    };
  }

  const rectBuffer = gl.createBuffer()!;
  const glyphBuffer = gl.createBuffer()!;
  const atlasTexture = gl.createTexture()!;

  const rectPosLoc = gl.getAttribLocation(rectProgram, "a_pos");
  const rectColorLoc = gl.getAttribLocation(rectProgram, "a_color");
  const rectResLoc = gl.getUniformLocation(rectProgram, "u_resolution");

  const glyphPosLoc = gl.getAttribLocation(glyphProgram, "a_pos");
  const glyphUvLoc = gl.getAttribLocation(glyphProgram, "a_uv");
  const glyphColorLoc = gl.getAttribLocation(glyphProgram, "a_color");
  const glyphResLoc = gl.getUniformLocation(glyphProgram, "u_resolution");
  const glyphTexLoc = gl.getUniformLocation(glyphProgram, "u_texture");

  let uploadedAtlasVersion = -1;
  let uploadedAtlasCanvas: HTMLCanvasElement | null = null;

  gl.disable(gl.DEPTH_TEST);
  gl.disable(gl.CULL_FACE);
  gl.enable(gl.BLEND);
  gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
  gl.bindTexture(gl.TEXTURE_2D, atlasTexture);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.NEAREST);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

  function ensureAtlas(
    atlasCanvas: HTMLCanvasElement,
    version: number,
  ): boolean {
    if (
      atlasCanvas === uploadedAtlasCanvas &&
      version >>> 0 === uploadedAtlasVersion >>> 0
    ) {
      return true;
    }
    gl!.bindTexture(gl!.TEXTURE_2D, atlasTexture);
    gl!.texImage2D(
      gl!.TEXTURE_2D,
      0,
      gl!.RGBA,
      gl!.RGBA,
      gl!.UNSIGNED_BYTE,
      atlasCanvas,
    );
    uploadedAtlasCanvas = atlasCanvas;
    uploadedAtlasVersion = version >>> 0;
    return true;
  }

  function drawColoredTriangles(data: Float32Array): void {
    if (!data.length) return;
    const floatsPerVert = 6;
    const totalVerts = data.length / floatsPerVert;
    gl!.useProgram(rectProgram);
    gl!.bindBuffer(gl!.ARRAY_BUFFER, rectBuffer);
    gl!.enableVertexAttribArray(rectPosLoc);
    gl!.enableVertexAttribArray(rectColorLoc);
    gl!.uniform2f(rectResLoc, canvas.width, canvas.height);
    for (let off = 0; off < totalVerts; off += MAX_BATCH_VERTS) {
      const count = Math.min(MAX_BATCH_VERTS, totalVerts - off);
      const slice = data.subarray(
        off * floatsPerVert,
        (off + count) * floatsPerVert,
      );
      gl!.bufferData(gl!.ARRAY_BUFFER, slice, gl!.DYNAMIC_DRAW);
      gl!.vertexAttribPointer(rectPosLoc, 2, gl!.FLOAT, false, 24, 0);
      gl!.vertexAttribPointer(rectColorLoc, 4, gl!.FLOAT, false, 24, 8);
      gl!.drawArrays(gl!.TRIANGLES, 0, count);
    }
  }

  function drawSolidRect(
    x1: number,
    y1: number,
    x2: number,
    y2: number,
    r: number,
    g: number,
    b: number,
    a: number,
  ): void {
    drawColoredTriangles(
      new Float32Array([
        x1,
        y1,
        r,
        g,
        b,
        a,
        x2,
        y1,
        r,
        g,
        b,
        a,
        x1,
        y2,
        r,
        g,
        b,
        a,
        x1,
        y2,
        r,
        g,
        b,
        a,
        x2,
        y1,
        r,
        g,
        b,
        a,
        x2,
        y2,
        r,
        g,
        b,
        a,
      ]),
    );
  }

  function renderRectangles(bgOps: Uint32Array, cell: CellMetrics): void {
    if (!bgOps.length) return;
    const data = new Float32Array((bgOps.length / BG_OP_STRIDE) * 36);
    let offset = 0;
    for (let i = 0; i < bgOps.length; i += BG_OP_STRIDE) {
      const row = bgOps[i];
      const col = bgOps[i + 1];
      const colSpan = bgOps[i + 2];
      const packed = bgOps[i + 3];
      const x1 = col * cell.pw;
      const y1 = row * cell.ph;
      const x2 = x1 + colSpan * cell.pw;
      const y2 = y1 + cell.ph;
      const r = ((packed >>> 16) & 0xff) / 255;
      const g = ((packed >>> 8) & 0xff) / 255;
      const b = (packed & 0xff) / 255;
      data.set(
        [
          x1,
          y1,
          r,
          g,
          b,
          1,
          x2,
          y1,
          r,
          g,
          b,
          1,
          x1,
          y2,
          r,
          g,
          b,
          1,
          x1,
          y2,
          r,
          g,
          b,
          1,
          x2,
          y1,
          r,
          g,
          b,
          1,
          x2,
          y2,
          r,
          g,
          b,
          1,
        ],
        offset,
      );
      offset += 36;
    }
    drawColoredTriangles(data);
  }

  function renderCursor(
    cursorVisible: boolean,
    cursorCol: number,
    cursorRow: number,
    cursorStyle: number,
    cursorBlinkOn: boolean,
    cell: CellMetrics,
  ): void {
    if (!cursorVisible) return;
    const blinks =
      cursorStyle === 0 ||
      cursorStyle === 1 ||
      cursorStyle === 3 ||
      cursorStyle === 5;
    if (blinks && !cursorBlinkOn) return;
    const x1 = cursorCol * cell.pw;
    const y1 = cursorRow * cell.ph;
    if (cursorStyle === 3 || cursorStyle === 4) {
      const h = Math.max(1, Math.round(cell.ph * 0.12));
      drawSolidRect(
        x1,
        y1 + cell.ph - h,
        x1 + cell.pw,
        y1 + cell.ph,
        0.8,
        0.8,
        0.8,
        0.8,
      );
    } else if (cursorStyle === 5 || cursorStyle === 6) {
      const w = Math.max(1, Math.round(cell.pw * 0.12));
      drawSolidRect(x1, y1, x1 + w, y1 + cell.ph, 0.8, 0.8, 0.8, 0.8);
    } else {
      drawSolidRect(x1, y1, x1 + cell.pw, y1 + cell.ph, 0.8, 0.8, 0.8, 0.5);
    }
  }

  function renderGlyphs(
    glyphOps: Uint32Array,
    atlasCanvas: HTMLCanvasElement,
    atlasVersion: number,
    cell: CellMetrics,
  ): void {
    if (!glyphOps.length || !ensureAtlas(atlasCanvas, atlasVersion)) return;
    const atlasWidth = atlasCanvas.width || 1;
    const atlasHeight = atlasCanvas.height || 1;
    const floatsPerVert = 8;
    const vertsPerGlyph = 6;
    const data = new Float32Array(
      (glyphOps.length / GLYPH_OP_STRIDE) * vertsPerGlyph * floatsPerVert,
    );
    let offset = 0;
    for (let i = 0; i < glyphOps.length; i += GLYPH_OP_STRIDE) {
      const srcX = glyphOps[i];
      const srcY = glyphOps[i + 1];
      const srcW = glyphOps[i + 2];
      const srcH = glyphOps[i + 3];
      const row = glyphOps[i + 4];
      const col = glyphOps[i + 5];
      const colSpan = glyphOps[i + 6];
      const packed = glyphOps[i + 7];
      const dx1 = col * cell.pw;
      const dy1 = row * cell.ph;
      const dx2 = dx1 + colSpan * cell.pw;
      const dy2 = dy1 + cell.ph;
      const u1 = srcX / atlasWidth;
      const v1 = srcY / atlasHeight;
      const u2 = (srcX + srcW) / atlasWidth;
      const v2 = (srcY + srcH) / atlasHeight;
      const r = ((packed >>> 16) & 0xff) / 255;
      const g = ((packed >>> 8) & 0xff) / 255;
      const b = (packed & 0xff) / 255;
      data.set(
        [
          dx1,
          dy1,
          u1,
          v1,
          r,
          g,
          b,
          1,
          dx2,
          dy1,
          u2,
          v1,
          r,
          g,
          b,
          1,
          dx1,
          dy2,
          u1,
          v2,
          r,
          g,
          b,
          1,
          dx1,
          dy2,
          u1,
          v2,
          r,
          g,
          b,
          1,
          dx2,
          dy1,
          u2,
          v1,
          r,
          g,
          b,
          1,
          dx2,
          dy2,
          u2,
          v2,
          r,
          g,
          b,
          1,
        ],
        offset,
      );
      offset += vertsPerGlyph * floatsPerVert;
    }
    const totalVerts = data.length / floatsPerVert;
    const stride = floatsPerVert * 4;
    gl!.useProgram(glyphProgram);
    gl!.bindBuffer(gl!.ARRAY_BUFFER, glyphBuffer);
    gl!.enableVertexAttribArray(glyphPosLoc);
    gl!.enableVertexAttribArray(glyphUvLoc);
    gl!.enableVertexAttribArray(glyphColorLoc);
    gl!.uniform2f(glyphResLoc, canvas.width, canvas.height);
    gl!.activeTexture(gl!.TEXTURE0);
    gl!.bindTexture(gl!.TEXTURE_2D, atlasTexture);
    gl!.uniform1i(glyphTexLoc, 0);
    for (let off = 0; off < totalVerts; off += MAX_BATCH_VERTS) {
      const count = Math.min(MAX_BATCH_VERTS, totalVerts - off);
      const slice = data.subarray(
        off * floatsPerVert,
        (off + count) * floatsPerVert,
      );
      gl!.bufferData(gl!.ARRAY_BUFFER, slice, gl!.DYNAMIC_DRAW);
      gl!.vertexAttribPointer(glyphPosLoc, 2, gl!.FLOAT, false, stride, 0);
      gl!.vertexAttribPointer(glyphUvLoc, 2, gl!.FLOAT, false, stride, 8);
      gl!.vertexAttribPointer(glyphColorLoc, 4, gl!.FLOAT, false, stride, 16);
      gl!.drawArrays(gl!.TRIANGLES, 0, count);
    }
  }

  return {
    supported: true,
    resize(width: number, height: number) {
      if (canvas.width !== width) canvas.width = width;
      if (canvas.height !== height) canvas.height = height;
    },
    render(
      bgOps: Uint32Array,
      glyphOps: Uint32Array,
      atlasCanvas: HTMLCanvasElement | undefined,
      atlasVersion: number,
      cursorVisible: boolean,
      cursorCol: number,
      cursorRow: number,
      cursorStyle: number,
      cursorBlinkOn: boolean,
      cell: CellMetrics,
      bgColor: [number, number, number],
    ) {
      gl!.viewport(0, 0, canvas.width, canvas.height);
      gl!.clearColor(bgColor[0] / 255, bgColor[1] / 255, bgColor[2] / 255, 1);
      gl!.clear(gl!.COLOR_BUFFER_BIT);
      renderRectangles(bgOps, cell);
      if (atlasCanvas) {
        renderGlyphs(glyphOps, atlasCanvas, atlasVersion, cell);
      }
      renderCursor(
        cursorVisible,
        cursorCol,
        cursorRow,
        cursorStyle,
        cursorBlinkOn,
        cell,
      );
    },
    dispose() {
      gl!.deleteBuffer(rectBuffer);
      gl!.deleteBuffer(glyphBuffer);
      gl!.deleteTexture(atlasTexture);
      gl!.deleteProgram(rectProgram);
      gl!.deleteProgram(glyphProgram);
    },
  };
}

// ---------------------------------------------------------------------------
// Keyboard encoding (ported from web/index.html keyToBytes)
// ---------------------------------------------------------------------------

const encoder = new TextEncoder();

function keyToBytes(e: KeyboardEvent, appCursor: boolean): Uint8Array | null {
  if (e.ctrlKey && !e.altKey && !e.metaKey) {
    const kc = e.key.charCodeAt(0);
    if (e.key.length === 1 && kc >= 1 && kc <= 26) return new Uint8Array([kc]);
    if (e.key.length === 1) {
      const code = e.key.toLowerCase().charCodeAt(0);
      if (code >= 97 && code <= 122) return new Uint8Array([code - 96]);
      if (e.key === "[") return new Uint8Array([0x1b]);
      if (e.key === "\\") return new Uint8Array([0x1c]);
      if (e.key === "]") return new Uint8Array([0x1d]);
    }
    // Fallback: use e.code when e.key is unhelpful (e.g. macOS Ctrl+letter)
    if (e.code && e.code.startsWith("Key")) {
      const cc = e.code.charCodeAt(3);
      if (cc >= 65 && cc <= 90) return new Uint8Array([cc - 64]);
    }
    if (e.code === "BracketLeft") return new Uint8Array([0x1b]);
    if (e.code === "Backslash") return new Uint8Array([0x1c]);
    if (e.code === "BracketRight") return new Uint8Array([0x1d]);
  }

  if (e.ctrlKey && e.shiftKey && !e.altKey && !e.metaKey) {
    if (e.key === "?") return new Uint8Array([0x7f]);
    if (e.key === " " || e.key === "@") return new Uint8Array([0x00]);
  }

  const arrows: Record<string, string> = {
    ArrowUp: "A",
    ArrowDown: "B",
    ArrowRight: "C",
    ArrowLeft: "D",
  };
  if (arrows[e.key]) {
    const mod =
      (e.shiftKey ? 1 : 0) +
      (e.altKey ? 2 : 0) +
      (e.ctrlKey ? 4 : 0) +
      (e.metaKey ? 8 : 0);
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${arrows[e.key]}`);
    const prefix = appCursor ? "\x1bO" : "\x1b[";
    return encoder.encode(prefix + arrows[e.key]);
  }

  const mod =
    (e.shiftKey ? 1 : 0) +
    (e.altKey ? 2 : 0) +
    (e.ctrlKey ? 4 : 0) +
    (e.metaKey ? 8 : 0);

  const tilde: Record<string, string> = {
    PageUp: "5",
    PageDown: "6",
    Delete: "3",
    Insert: "2",
  };
  if (tilde[e.key]) {
    if (mod) return encoder.encode(`\x1b[${tilde[e.key]};${mod + 1}~`);
    return encoder.encode(`\x1b[${tilde[e.key]}~`);
  }

  const he: Record<string, string> = { Home: "H", End: "F" };
  if (he[e.key]) {
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${he[e.key]}`);
    return encoder.encode(`\x1b[${he[e.key]}`);
  }

  const f14: Record<string, string> = { F1: "P", F2: "Q", F3: "R", F4: "S" };
  if (f14[e.key]) {
    if (mod) return encoder.encode(`\x1b[1;${mod + 1}${f14[e.key]}`);
    return encoder.encode(`\x1bO${f14[e.key]}`);
  }

  const fkeys: Record<string, string> = {
    F5: "15",
    F6: "17",
    F7: "18",
    F8: "19",
    F9: "20",
    F10: "21",
    F11: "23",
    F12: "24",
  };
  if (fkeys[e.key]) {
    if (mod) return encoder.encode(`\x1b[${fkeys[e.key]};${mod + 1}~`);
    return encoder.encode(`\x1b[${fkeys[e.key]}~`);
  }

  const simple: Record<string, string> = {
    Enter: "\r",
    Backspace: "\x7f",
    Tab: "\t",
    Escape: "\x1b",
  };
  if (simple[e.key]) return encoder.encode(simple[e.key]);

  if (e.altKey && !e.ctrlKey && !e.metaKey && e.key.length === 1) {
    const code = e.key.charCodeAt(0);
    if (code >= 0x20 && code <= 0x7e) return encoder.encode("\x1b" + e.key);
    return encoder.encode(e.key);
  }

  if (e.key.length === 1 && !e.ctrlKey && !e.metaKey && !e.altKey) {
    return encoder.encode(e.key);
  }

  return null;
}

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
    const {
      transport,
      ptyId,
      fontFamily = DEFAULT_FONT,
      fontSize = DEFAULT_FONT_SIZE,
      className,
      style,
      palette,
      readOnly = false,
      store,
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
        if (t && terminalRef.current !== t) {
          terminalRef.current = t;
          needsRenderRef.current = true;
        }
        if (!t) return;
        needsRenderRef.current = true;
        reconcilePrediction();
        if (readOnly) syncReadOnlySize(t);
      });
      if (ptyId !== null) {
        const t = store.getTerminal(ptyId);
        if (t) {
          terminalRef.current = t;
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
    }, [transport, ptyId, readOnly, store, reconcilePrediction]);

    const sendInput = useCallback(
      (id: number, data: Uint8Array) => {
        const msg = new Uint8Array(3 + data.length);
        msg[0] = C2S_INPUT;
        msg[1] = id & 0xff;
        msg[2] = (id >> 8) & 0xff;
        msg.set(data, 3);
        transport.send(msg);
      },
      [transport],
    );

    const sendResize = useCallback(
      (id: number, rows: number, cols: number) => {
        const msg = new Uint8Array(7);
        msg[0] = C2S_RESIZE;
        msg[1] = id & 0xff;
        msg[2] = (id >> 8) & 0xff;
        msg[3] = rows & 0xff;
        msg[4] = (rows >> 8) & 0xff;
        msg[5] = cols & 0xff;
        msg[6] = (cols >> 8) & 0xff;
        transport.send(msg);
      },
      [transport],
    );

    const sendScroll = useCallback(
      (id: number, offset: number) => {
        const msg = new Uint8Array(7);
        msg[0] = C2S_SCROLL;
        msg[1] = id & 0xff;
        msg[2] = (id >> 8) & 0xff;
        msg[3] = offset & 0xff;
        msg[4] = (offset >> 8) & 0xff;
        msg[5] = (offset >> 16) & 0xff;
        msg[6] = (offset >> 24) & 0xff;
        transport.send(msg);
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
        const t = store.getTerminal(ptyId);
        if (t) {
          terminalRef.current = t;
          if (!readOnly) {
            const cell = cellRef.current;
            t.set_cell_size(cell.pw, cell.ph);
            if (typeof t.set_font_family === "function") t.set_font_family(fontFamily);
          }
          needsRenderRef.current = true;
          needsRenderRef.current = true;
        }
      } else {
        terminalRef.current = null;
      }
    }, [wasmReady, ptyId, store, fontFamily, readOnly]);

    // -----------------------------------------------------------------------
    // Palette changes
    // -----------------------------------------------------------------------

    useEffect(() => {
      paletteRef.current = palette;
      const t = terminalRef.current;
      if (!t || !palette) return;
      t.set_default_colors(...palette.fg, ...palette.bg);
      for (let i = 0; i < 16; i++) t.set_ansi_color(i, ...palette.ansi[i]);
      needsRenderRef.current = true;
    }, [palette]);

    // -----------------------------------------------------------------------
    // Resize observer
    // -----------------------------------------------------------------------

    useEffect(() => {
      const container = containerRef.current;
      if (!container || readOnly) return;

      let resizeTimer: ReturnType<typeof setTimeout> | null = null;
      let pendingRows = colsRef.current;
      let pendingCols = rowsRef.current;
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
            t.glyph_atlas_version(),
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
    }, [fontFamily]);

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
      let selStart: { row: number; col: number } | null = null;
      let selEnd: { row: number; col: number } | null = null;
      const selOverlays: HTMLElement[] = [];

      function clearSelection() {
        for (const el of selOverlays) el.remove();
        selOverlays.length = 0;
        selStart = selEnd = null;
      }

      function drawSelection() {
        for (const el of selOverlays) el.remove();
        selOverlays.length = 0;
        if (!selStart || !selEnd) return;
        let sr = selStart.row, sc = selStart.col;
        let er = selEnd.row, ec = selEnd.col;
        if (sr > er || (sr === er && sc > ec)) {
          [sr, sc, er, ec] = [er, ec, sr, sc];
        }
        const rect = canvas!.getBoundingClientRect();
        const cell = cellRef.current;
        for (let r = sr; r <= er; r++) {
          const c0 = r === sr ? sc : 0;
          const c1 = r === er ? ec : colsRef.current - 1;
          const el = document.createElement("div");
          el.style.cssText =
            "position:fixed;background:rgba(100,150,255,0.3);pointer-events:none;z-index:1";
          el.style.left = rect.left + c0 * cell.w + "px";
          el.style.top = rect.top + r * cell.h + "px";
          el.style.width = (c1 - c0 + 1) * cell.w + "px";
          el.style.height = cell.h + "px";
          document.body.appendChild(el);
          selOverlays.push(el);
        }
      }

      function copySelection() {
        if (!selStart || !selEnd) return;
        const t = terminalRef.current;
        if (!t) return;
        let sr = selStart.row, sc = selStart.col;
        let er = selEnd.row, ec = selEnd.col;
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
          selStart = mouseToCell(e);
          selEnd = selStart;
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
          selEnd = mouseToCell(e);
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
          selEnd = mouseToCell(e);
          drawSelection();
          if (
            selStart &&
            selEnd &&
            (selStart.row !== selEnd.row || selStart.col !== selEnd.col)
          ) {
            copySelection();
          }
          clearSelection();
        }
        inputRef.current?.focus();
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

      const handleContextMenu = (e: MouseEvent) => {
        const t = terminalRef.current;
        if (t && t.mouse_mode() > 0) e.preventDefault();
      };

      const handleClick = () => {
        inputRef.current?.focus();
      };

      canvas.addEventListener("mousedown", handleMouseDown);
      canvas.addEventListener("mousemove", handleMouseMove);
      window.addEventListener("mouseup", handleMouseUp);
      canvas.addEventListener("wheel", handleWheel, { passive: false });
      canvas.addEventListener("contextmenu", handleContextMenu);
      canvas.addEventListener("click", handleClick);

      return () => {
        canvas.removeEventListener("mousedown", handleMouseDown);
        canvas.removeEventListener("mousemove", handleMouseMove);
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
