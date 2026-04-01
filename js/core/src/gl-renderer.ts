import type { CellMetrics } from "./measure";

const MAX_BATCH_VERTS = 65532;
const GLYPH_FLOATS_PER_VERT = 8;

const RECT_VS = `#version 300 es
in vec2 a_pos;
in vec4 a_color;
uniform vec2 u_resolution;
out vec4 v_color;

void main() {
    vec2 zeroToOne = a_pos / u_resolution;
    vec2 zeroToTwo = zeroToOne * 2.0;
    vec2 clip = zeroToTwo - 1.0;
    gl_Position = vec4(clip * vec2(1.0, -1.0), 0.0, 1.0);
    v_color = a_color;
}
`;

const RECT_FS = `#version 300 es
precision mediump float;
in vec4 v_color;
out vec4 fragColor;

void main() {
    fragColor = vec4(v_color.rgb * v_color.a, v_color.a);
}
`;

const GLYPH_VS = `#version 300 es
in vec2 a_pos;
in vec2 a_uv;
in vec4 a_color;
uniform vec2 u_resolution;
out vec2 v_uv;
out vec4 v_color;

void main() {
    vec2 zeroToOne = a_pos / u_resolution;
    vec2 zeroToTwo = zeroToOne * 2.0;
    vec2 clip = zeroToTwo - 1.0;
    gl_Position = vec4(clip * vec2(1.0, -1.0), 0.0, 1.0);
    v_uv = a_uv;
    v_color = a_color;
}
`;

const GLYPH_FS = `#version 300 es
precision mediump float;
in vec2 v_uv;
in vec4 v_color;
uniform sampler2D u_texture;
out vec4 fragColor;

void main() {
    vec4 tex = texture(u_texture, v_uv);
    float minC = min(tex.r, min(tex.g, tex.b));
    float maxC = max(tex.r, max(tex.g, tex.b));
    float isGray = step(maxC - minC, 0.02);
    vec3 tinted = v_color.rgb * tex.a;
    fragColor = vec4(mix(tex.rgb, tinted, isGray), tex.a);
}
`;

function compileShader(
  gl: WebGL2RenderingContext,
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
  gl: WebGL2RenderingContext,
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
  maxDimension: number;
  resize(width: number, height: number): void;
  render(
    bgVerts: Float32Array,
    glyphVerts: Float32Array,
    atlasCanvas: HTMLCanvasElement | undefined,
    atlasVersion: number,
    cursorVisible: boolean,
    cursorCol: number,
    cursorRow: number,
    cursorStyle: number,
    cursorBlinkOn: boolean,
    cell: CellMetrics,
    bgColor: [number, number, number],
    focused?: boolean,
  ): void;
  dispose(): void;
}

const UNSUPPORTED: GlRenderer = {
  supported: false,
  maxDimension: 0,
  resize() {},
  render() {},
  dispose() {},
};

export function createGlRenderer(canvas: HTMLCanvasElement): GlRenderer {
  const gl = canvas.getContext("webgl2", {
    alpha: true,
    antialias: false,
    depth: false,
    stencil: false,
    premultipliedAlpha: true,
  }) as WebGL2RenderingContext | null;

  if (!gl) return { ...UNSUPPORTED };

  const rectProgram = createProgram(gl, RECT_VS, RECT_FS);
  const glyphProgram = createProgram(gl, GLYPH_VS, GLYPH_FS);

  if (!rectProgram || !glyphProgram) {
    return { ...UNSUPPORTED };
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

  const maxDim = (gl.getParameter(gl.MAX_RENDERBUFFER_SIZE) as number) || 4096;

  canvas.addEventListener("webglcontextlost", (e) => {
    e.preventDefault();
    console.warn("blit: WebGL context lost");
  });
  canvas.addEventListener("webglcontextrestored", () => {
    console.warn("blit: WebGL context restored");
  });

  gl.disable(gl.DEPTH_TEST);
  gl.disable(gl.CULL_FACE);
  gl.enable(gl.BLEND);
  gl.blendFunc(gl.ONE, gl.ONE_MINUS_SRC_ALPHA);
  gl.pixelStorei(gl.UNPACK_PREMULTIPLY_ALPHA_WEBGL, true);
  gl.bindTexture(gl.TEXTURE_2D, atlasTexture);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);

  let lastAtlasCanvas: HTMLCanvasElement | null = null;
  let lastAtlasVersion = -1;

  function uploadAtlas(atlasCanvas: HTMLCanvasElement, version: number): void {
    if (atlasCanvas === lastAtlasCanvas && version === lastAtlasVersion) return;
    lastAtlasCanvas = atlasCanvas;
    lastAtlasVersion = version;
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

  function renderCursor(
    cursorVisible: boolean,
    cursorCol: number,
    cursorRow: number,
    cursorStyle: number,
    cursorBlinkOn: boolean,
    cell: CellMetrics,
    focused: boolean,
  ): void {
    if (!cursorVisible) return;
    const x1 = cursorCol * cell.pw;
    const y1 = cursorRow * cell.ph;

    if (!focused) {
      // Unfocused: non-blinking outline.
      const t = Math.max(1, Math.round(cell.pw * 0.08));
      drawSolidRect(x1, y1, x1 + cell.pw, y1 + t, 0.6, 0.6, 0.6, 0.6);
      drawSolidRect(
        x1,
        y1 + cell.ph - t,
        x1 + cell.pw,
        y1 + cell.ph,
        0.6,
        0.6,
        0.6,
        0.6,
      );
      drawSolidRect(x1, y1, x1 + t, y1 + cell.ph, 0.6, 0.6, 0.6, 0.6);
      drawSolidRect(
        x1 + cell.pw - t,
        y1,
        x1 + cell.pw,
        y1 + cell.ph,
        0.6,
        0.6,
        0.6,
        0.6,
      );
      return;
    }

    const blinks =
      cursorStyle === 0 ||
      cursorStyle === 1 ||
      cursorStyle === 3 ||
      cursorStyle === 5;
    if (blinks && !cursorBlinkOn) return;
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

  function uploadAndDrawGlyphs(
    data: Float32Array,
    atlasCanvas: HTMLCanvasElement,
    atlasVersion: number,
  ): void {
    if (!data.length || !atlasCanvas) return;
    uploadAtlas(atlasCanvas, atlasVersion);
    const totalVerts = data.length / GLYPH_FLOATS_PER_VERT;
    const stride = GLYPH_FLOATS_PER_VERT * 4;
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
        off * GLYPH_FLOATS_PER_VERT,
        (off + count) * GLYPH_FLOATS_PER_VERT,
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
    maxDimension: maxDim,
    resize(width: number, height: number) {
      const w = Math.min(width, maxDim);
      const h = Math.min(height, maxDim);
      if (canvas.width !== w) canvas.width = w;
      if (canvas.height !== h) canvas.height = h;
    },
    render(
      bgVerts: Float32Array,
      glyphVerts: Float32Array,
      atlasCanvas: HTMLCanvasElement | undefined,
      atlasVersion: number,
      cursorVisible: boolean,
      cursorCol: number,
      cursorRow: number,
      cursorStyle: number,
      cursorBlinkOn: boolean,
      cell: CellMetrics,
      bgColor: [number, number, number],
      focused = true,
    ) {
      if (gl!.isContextLost()) return;
      gl!.viewport(0, 0, canvas.width, canvas.height);
      gl!.clearColor(bgColor[0] / 255, bgColor[1] / 255, bgColor[2] / 255, 1);
      gl!.clear(gl!.COLOR_BUFFER_BIT);
      drawColoredTriangles(bgVerts);
      if (atlasCanvas) {
        uploadAndDrawGlyphs(glyphVerts, atlasCanvas, atlasVersion);
      }
      renderCursor(
        cursorVisible,
        cursorCol,
        cursorRow,
        cursorStyle,
        cursorBlinkOn,
        cell,
        focused,
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
