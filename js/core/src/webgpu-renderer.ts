import type { CellMetrics } from "./measure";
import type { GlRenderer } from "./gl-renderer";

// ---------------------------------------------------------------------------
// WGSL shaders
// ---------------------------------------------------------------------------

const RECT_WGSL = /* wgsl */ `
struct Uniforms { resolution: vec2f }
@group(0) @binding(0) var<uniform> u: Uniforms;

struct VIn {
  @location(0) pos:   vec2f,
  @location(1) color: vec4f,
}
struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) color: vec4f,
}

@vertex fn vs(v: VIn) -> VOut {
  let clip = (v.pos / u.resolution) * 2.0 - 1.0;
  return VOut(vec4f(clip.x, -clip.y, 0.0, 1.0), v.color);
}

@fragment fn fs(v: VOut) -> @location(0) vec4f {
  return vec4f(v.color.rgb * v.color.a, v.color.a);
}
`;

const GLYPH_WGSL = /* wgsl */ `
struct Uniforms { resolution: vec2f }
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlasTex: texture_2d<f32>;
@group(0) @binding(2) var atlasSamp: sampler;

struct VIn {
  @location(0) pos:   vec2f,
  @location(1) uv:    vec2f,
  @location(2) color: vec4f,
}
struct VOut {
  @builtin(position) pos: vec4f,
  @location(0) uv:    vec2f,
  @location(1) color: vec4f,
}

@vertex fn vs(v: VIn) -> VOut {
  let clip = (v.pos / u.resolution) * 2.0 - 1.0;
  return VOut(vec4f(clip.x, -clip.y, 0.0, 1.0), v.uv, v.color);
}

@fragment fn fs(v: VOut) -> @location(0) vec4f {
  let tex = textureSample(atlasTex, atlasSamp, v.uv);
  let minC = min(tex.r, min(tex.g, tex.b));
  let maxC = max(tex.r, max(tex.g, tex.b));
  let isGray = step(maxC - minC, 0.02);
  let tinted = v.color.rgb * tex.a;
  return vec4f(mix(tex.rgb, tinted, isGray), tex.a);
}
`;

// ---------------------------------------------------------------------------
// WebGPU usage-flag constants (spec-defined, stable across browsers).
// TypeScript's DOM lib exposes the types but not the namespace objects.
// ---------------------------------------------------------------------------

const BUF_VERTEX = 0x0020;
const BUF_UNIFORM = 0x0040;
const BUF_COPY_DST = 0x0008;

const TEX_BINDING = 0x04;
const TEX_COPY_DST = 0x02;
const TEX_RENDER_ATTACHMENT = 0x10;

const STAGE_VERTEX = 0x1;
const STAGE_FRAGMENT = 0x2;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/** Round up to the next multiple of `a`. */
function alignUp(n: number, a: number): number {
  return (n + a - 1) & ~(a - 1);
}

/** Ensure a GPUBuffer is at least `need` bytes; recreate if not. */
function ensureBuffer(
  device: GPUDevice,
  buf: GPUBuffer | null,
  need: number,
  usage: GPUBufferUsageFlags,
): GPUBuffer {
  if (buf && buf.size >= need) return buf;
  buf?.destroy();
  // Grow to 1.5x to reduce re-allocations on gradual growth.
  const size = alignUp(Math.max(need, 256), 256);
  return device.createBuffer({ size, usage });
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/**
 * Attempt to create a WebGPU-backed renderer for the given canvas.
 * Returns `null` if WebGPU is unavailable or initialisation fails.
 * The returned promise resolves to a `GlRenderer` (same interface as the
 * WebGL2 renderer) so callers can treat it as a drop-in replacement.
 */
export async function createWebGpuRenderer(
  canvas: HTMLCanvasElement,
): Promise<GlRenderer | null> {
  if (typeof navigator === "undefined" || !navigator.gpu) return null;

  let adapter: GPUAdapter | null;
  try {
    adapter = await navigator.gpu.requestAdapter({
      powerPreference: "high-performance",
    });
  } catch {
    return null;
  }
  if (!adapter) return null;

  let device: GPUDevice;
  try {
    device = await adapter.requestDevice();
  } catch {
    return null;
  }

  const ctx = canvas.getContext("webgpu") as GPUCanvasContext | null;
  if (!ctx) return null;

  const format = navigator.gpu.getPreferredCanvasFormat();
  ctx.configure({
    device,
    format,
    alphaMode: "premultiplied",
  });

  // --- uniform buffer (shared: vec2f resolution, 8 bytes padded to 16) ---
  const uniformBuf = device.createBuffer({
    size: 16,
    usage: BUF_UNIFORM | BUF_COPY_DST,
  });

  // --- rect pipeline ---
  const rectModule = device.createShaderModule({ code: RECT_WGSL });
  const rectBindGroupLayout = device.createBindGroupLayout({
    entries: [
      { binding: 0, visibility: STAGE_VERTEX, buffer: { type: "uniform" } },
    ],
  });
  const rectPipeline = device.createRenderPipeline({
    layout: device.createPipelineLayout({
      bindGroupLayouts: [rectBindGroupLayout],
    }),
    vertex: {
      module: rectModule,
      buffers: [
        {
          arrayStride: 24, // 6 floats: pos(2) + color(4)
          attributes: [
            { shaderLocation: 0, offset: 0, format: "float32x2" },
            { shaderLocation: 1, offset: 8, format: "float32x4" },
          ],
        },
      ],
    },
    fragment: {
      module: rectModule,
      targets: [
        {
          format,
          blend: {
            color: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
            alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
          },
        },
      ],
    },
    primitive: { topology: "triangle-list" },
  });
  const rectBindGroup = device.createBindGroup({
    layout: rectBindGroupLayout,
    entries: [{ binding: 0, resource: { buffer: uniformBuf } }],
  });

  // --- glyph pipeline ---
  const glyphModule = device.createShaderModule({ code: GLYPH_WGSL });
  const glyphBindGroupLayout = device.createBindGroupLayout({
    entries: [
      { binding: 0, visibility: STAGE_VERTEX, buffer: { type: "uniform" } },
      {
        binding: 1,
        visibility: STAGE_FRAGMENT,
        texture: { sampleType: "float" },
      },
      {
        binding: 2,
        visibility: STAGE_FRAGMENT,
        sampler: { type: "filtering" },
      },
    ],
  });
  const glyphPipeline = device.createRenderPipeline({
    layout: device.createPipelineLayout({
      bindGroupLayouts: [glyphBindGroupLayout],
    }),
    vertex: {
      module: glyphModule,
      buffers: [
        {
          arrayStride: 32, // 8 floats: pos(2) + uv(2) + color(4)
          attributes: [
            { shaderLocation: 0, offset: 0, format: "float32x2" },
            { shaderLocation: 1, offset: 8, format: "float32x2" },
            { shaderLocation: 2, offset: 16, format: "float32x4" },
          ],
        },
      ],
    },
    fragment: {
      module: glyphModule,
      targets: [
        {
          format,
          blend: {
            color: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
            alpha: { srcFactor: "one", dstFactor: "one-minus-src-alpha" },
          },
        },
      ],
    },
    primitive: { topology: "triangle-list" },
  });
  const sampler = device.createSampler({
    minFilter: "linear",
    magFilter: "linear",
    addressModeU: "clamp-to-edge",
    addressModeV: "clamp-to-edge",
  });

  // --- dynamic state ---
  let rectVB: GPUBuffer | null = null;
  let glyphVB: GPUBuffer | null = null;
  let cursorVB: GPUBuffer | null = null;
  let atlasTexture: GPUTexture | null = null;
  let glyphBindGroup: GPUBindGroup | null = null;
  let lastAtlasCanvas: HTMLCanvasElement | null = null;
  let lastAtlasVersion = -1;
  let lost = false;

  device.lost.then(() => {
    lost = true;
  });

  const maxDim = device.limits.maxTextureDimension2D ?? 8192;

  // --- atlas upload ---
  function uploadAtlas(atlasCanvas: HTMLCanvasElement, version: number): void {
    if (atlasCanvas === lastAtlasCanvas && version === lastAtlasVersion) return;
    lastAtlasCanvas = atlasCanvas;
    lastAtlasVersion = version;
    const w = atlasCanvas.width;
    const h = atlasCanvas.height;
    if (w === 0 || h === 0) return;

    // Recreate texture if size changed.
    if (
      !atlasTexture ||
      atlasTexture.width !== w ||
      atlasTexture.height !== h
    ) {
      atlasTexture?.destroy();
      atlasTexture = device.createTexture({
        size: { width: w, height: h },
        format: "rgba8unorm",
        usage: TEX_BINDING | TEX_COPY_DST | TEX_RENDER_ATTACHMENT,
      });
      // Recreate bind group with new texture view.
      glyphBindGroup = device.createBindGroup({
        layout: glyphBindGroupLayout,
        entries: [
          { binding: 0, resource: { buffer: uniformBuf } },
          { binding: 1, resource: atlasTexture.createView() },
          { binding: 2, resource: sampler },
        ],
      });
    }

    device.queue.copyExternalImageToTexture(
      { source: atlasCanvas },
      { texture: atlasTexture, premultipliedAlpha: true },
      { width: w, height: h },
    );
  }

  // --- cursor helpers (mirrors WebGL renderer exactly) ---

  /** Build cursor geometry into a Float32Array. Returns vertex count. */
  function buildCursorVerts(
    cursorVisible: boolean,
    cursorCol: number,
    cursorRow: number,
    cursorStyle: number,
    cursorBlinkOn: boolean,
    cell: CellMetrics,
    focused: boolean,
  ): Float32Array | null {
    if (!cursorVisible) return null;
    const x1 = cursorCol * cell.pw;
    const y1 = cursorRow * cell.ph;

    // Each rect = 6 verts * 6 floats = 36 floats. Max 4 rects for outline.
    const rects: number[] = [];
    function pushRect(
      rx1: number,
      ry1: number,
      rx2: number,
      ry2: number,
      r: number,
      g: number,
      b: number,
      a: number,
    ): void {
      rects.push(
        rx1,
        ry1,
        r,
        g,
        b,
        a,
        rx2,
        ry1,
        r,
        g,
        b,
        a,
        rx1,
        ry2,
        r,
        g,
        b,
        a,
        rx1,
        ry2,
        r,
        g,
        b,
        a,
        rx2,
        ry1,
        r,
        g,
        b,
        a,
        rx2,
        ry2,
        r,
        g,
        b,
        a,
      );
    }

    if (!focused) {
      const t = Math.max(1, Math.round(cell.pw * 0.08));
      pushRect(x1, y1, x1 + cell.pw, y1 + t, 0.6, 0.6, 0.6, 0.6);
      pushRect(
        x1,
        y1 + cell.ph - t,
        x1 + cell.pw,
        y1 + cell.ph,
        0.6,
        0.6,
        0.6,
        0.6,
      );
      pushRect(x1, y1, x1 + t, y1 + cell.ph, 0.6, 0.6, 0.6, 0.6);
      pushRect(
        x1 + cell.pw - t,
        y1,
        x1 + cell.pw,
        y1 + cell.ph,
        0.6,
        0.6,
        0.6,
        0.6,
      );
    } else {
      const blinks =
        cursorStyle === 0 ||
        cursorStyle === 1 ||
        cursorStyle === 3 ||
        cursorStyle === 5;
      if (blinks && !cursorBlinkOn) return null;
      if (cursorStyle === 3 || cursorStyle === 4) {
        const h = Math.max(1, Math.round(cell.ph * 0.12));
        pushRect(
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
        pushRect(x1, y1, x1 + w, y1 + cell.ph, 0.8, 0.8, 0.8, 0.8);
      } else {
        pushRect(x1, y1, x1 + cell.pw, y1 + cell.ph, 0.8, 0.8, 0.8, 0.5);
      }
    }

    return rects.length > 0 ? new Float32Array(rects) : null;
  }

  // --- GlRenderer implementation ---
  return {
    supported: true,
    backend: "webgpu" as const,
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
      if (lost) return;

      // Upload resolution uniform.
      device.queue.writeBuffer(
        uniformBuf,
        0,
        new Float32Array([canvas.width, canvas.height]),
      );

      // Upload atlas if needed.
      if (atlasCanvas) uploadAtlas(atlasCanvas, atlasVersion);

      const vbUsage = BUF_VERTEX | BUF_COPY_DST;

      // Grow vertex buffers if needed.
      if (bgVerts.byteLength > 0) {
        rectVB = ensureBuffer(device, rectVB, bgVerts.byteLength, vbUsage);
        device.queue.writeBuffer(rectVB, 0, bgVerts);
      }
      if (glyphVerts.byteLength > 0 && atlasCanvas) {
        glyphVB = ensureBuffer(device, glyphVB, glyphVerts.byteLength, vbUsage);
        device.queue.writeBuffer(glyphVB, 0, glyphVerts);
      }

      // Build cursor geometry.
      const cursorData = buildCursorVerts(
        cursorVisible,
        cursorCol,
        cursorRow,
        cursorStyle,
        cursorBlinkOn,
        cell,
        focused,
      );
      if (cursorData) {
        cursorVB = ensureBuffer(
          device,
          cursorVB,
          cursorData.byteLength,
          vbUsage,
        );
        device.queue.writeBuffer(cursorVB, 0, cursorData);
      }

      let texture: GPUTexture;
      try {
        texture = ctx.getCurrentTexture();
      } catch {
        return; // Canvas not visible or context lost.
      }
      const enc = device.createCommandEncoder();
      const pass = enc.beginRenderPass({
        colorAttachments: [
          {
            view: texture.createView(),
            loadOp: "clear",
            storeOp: "store",
            clearValue: {
              r: bgColor[0] / 255,
              g: bgColor[1] / 255,
              b: bgColor[2] / 255,
              a: 1,
            },
          },
        ],
      });

      // Background rects.
      if (bgVerts.length > 0 && rectVB) {
        const vertCount = bgVerts.length / 6;
        pass.setPipeline(rectPipeline);
        pass.setBindGroup(0, rectBindGroup);
        pass.setVertexBuffer(0, rectVB);
        pass.draw(vertCount);
      }

      // Glyph quads.
      if (glyphVerts.length > 0 && glyphVB && glyphBindGroup && atlasCanvas) {
        const vertCount = glyphVerts.length / 8;
        pass.setPipeline(glyphPipeline);
        pass.setBindGroup(0, glyphBindGroup);
        pass.setVertexBuffer(0, glyphVB);
        pass.draw(vertCount);
      }

      // Cursor.
      if (cursorData && cursorVB) {
        const vertCount = cursorData.length / 6;
        pass.setPipeline(rectPipeline);
        pass.setBindGroup(0, rectBindGroup);
        pass.setVertexBuffer(0, cursorVB);
        pass.draw(vertCount);
      }

      pass.end();
      device.queue.submit([enc.finish()]);
    },

    dispose() {
      rectVB?.destroy();
      glyphVB?.destroy();
      cursorVB?.destroy();
      atlasTexture?.destroy();
      uniformBuf.destroy();
      device.destroy();
      rectVB = null;
      glyphVB = null;
      cursorVB = null;
      atlasTexture = null;
      glyphBindGroup = null;
    },
  };
}
