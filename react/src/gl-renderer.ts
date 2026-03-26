import type { CellMetrics } from "./hooks/useBlitTerminal";

const BG_OP_STRIDE = 4;
const GLYPH_OP_STRIDE = 8;
const MAX_BATCH_VERTS = 65532;

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

export interface GlRenderer {
  supported: boolean;
  resize(width: number, height: number): void;
  render(
    bgOps: Uint32Array,
    glyphOps: Uint32Array,
    atlasCanvas: HTMLCanvasElement | undefined,
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

export function createGlRenderer(canvas: HTMLCanvasElement): GlRenderer {
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

  function uploadAtlas(atlasCanvas: HTMLCanvasElement): void {
    gl!.bindTexture(gl!.TEXTURE_2D, atlasTexture);
    gl!.texImage2D(
      gl!.TEXTURE_2D,
      0,
      gl!.RGBA,
      gl!.RGBA,
      gl!.UNSIGNED_BYTE,
      atlasCanvas,
    );
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
        x1, y1, r, g, b, a,
        x2, y1, r, g, b, a,
        x1, y2, r, g, b, a,
        x1, y2, r, g, b, a,
        x2, y1, r, g, b, a,
        x2, y2, r, g, b, a,
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
          x1, y1, r, g, b, 1,
          x2, y1, r, g, b, 1,
          x1, y2, r, g, b, 1,
          x1, y2, r, g, b, 1,
          x2, y1, r, g, b, 1,
          x2, y2, r, g, b, 1,
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
        x1, y1 + cell.ph - h, x1 + cell.pw, y1 + cell.ph,
        0.8, 0.8, 0.8, 0.8,
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
    cell: CellMetrics,
  ): void {
    if (!glyphOps.length || !atlasCanvas) return;
    uploadAtlas(atlasCanvas);
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
      const vertPad = srcH - cell.ph;
      const dy1 = row * cell.ph - vertPad;
      const dx2 = dx1 + colSpan * cell.pw;
      const dy2 = dy1 + srcH;
      const u1 = srcX / atlasWidth;
      const v1 = srcY / atlasHeight;
      const u2 = (srcX + srcW) / atlasWidth;
      const v2 = (srcY + srcH) / atlasHeight;
      const r = ((packed >>> 16) & 0xff) / 255;
      const g = ((packed >>> 8) & 0xff) / 255;
      const b = (packed & 0xff) / 255;
      data.set(
        [
          dx1, dy1, u1, v1, r, g, b, 1,
          dx2, dy1, u2, v1, r, g, b, 1,
          dx1, dy2, u1, v2, r, g, b, 1,
          dx1, dy2, u1, v2, r, g, b, 1,
          dx2, dy1, u2, v1, r, g, b, 1,
          dx2, dy2, u2, v2, r, g, b, 1,
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
        renderGlyphs(glyphOps, atlasCanvas, cell);
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
