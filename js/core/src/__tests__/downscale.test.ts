import { describe, it, expect, vi, beforeAll } from "vitest";
import { drawHalved, halve, halvings, octaveCeil } from "../downscale";

type Call = unknown[];

interface FakeCtx {
  canvas: HTMLCanvasElement;
  imageSmoothingEnabled: boolean;
  calls: Call[];
  drawImage(...args: Call): void;
}

beforeAll(() => {
  // jsdom has no 2D backend, and drawHalved falls back to a single tap when it
  // cannot get one — which is the path this file is *not* trying to test.
  // Installed once and never restored: the scratch buffers are memoised, so a
  // later restore would leave the module holding contexts nobody can inspect.
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(
    function (this: HTMLCanvasElement) {
      const ctx: FakeCtx = {
        canvas: this,
        imageSmoothingEnabled: false,
        calls: [],
        drawImage(...args) {
          this.calls.push(args);
        },
      };
      return ctx as unknown as CanvasRenderingContext2D;
    },
  );
});

function destination(): FakeCtx {
  return document
    .createElement("canvas")
    .getContext("2d") as unknown as FakeCtx;
}

function source(width: number, height: number): HTMLCanvasElement {
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  return canvas;
}

describe("halvings", () => {
  it("counts whole octaves of reduction and nothing more", () => {
    // Under 2:1 the browser's own bilinear tap is the correct filter, so the
    // chain stays empty and CSS keeps doing the work.
    expect(halvings(1920, 1080, 1920, 1080)).toBe(0);
    expect(halvings(1920, 1080, 1100, 620)).toBe(0);
    expect(halvings(1920, 1080, 960, 540)).toBe(1);
    expect(halvings(1920, 1080, 500, 281)).toBe(1);
    expect(halvings(1920, 1080, 480, 270)).toBe(2);
    expect(halvings(1920, 1080, 240, 135)).toBe(3);
  });

  it("measures the reduction the way object-fit: contain does", () => {
    // A wide frame in a tall box is bounded by width alone: the axis that has
    // to shrink most is the one that sets the scale.
    expect(halvings(1600, 200, 400, 4000)).toBe(2);
    expect(halvings(200, 1600, 4000, 400)).toBe(2);
  });

  it("treats a degenerate box as no reduction", () => {
    // A detached or display:none container measures zero; dividing by it would
    // ask for an infinite chain.
    expect(halvings(1920, 1080, 0, 0)).toBe(0);
    expect(halvings(0, 0, 100, 100)).toBe(0);
    expect(halvings(1920, 1080, -10, 50)).toBe(0);
  });

  it("caps the chain", () => {
    expect(halvings(1 << 20, 1 << 20, 1, 1)).toBe(6);
  });
});

describe("halve", () => {
  it("matches what the chain writes, and never reaches zero", () => {
    expect(halve(1920, 0)).toBe(1920);
    expect(halve(1920, 3)).toBe(240);
    expect(halve(1080, 3)).toBe(135);
    expect(halve(3, 6)).toBe(1);
  });
});

describe("octaveCeil", () => {
  it("rounds up to the next octave so small moves ask for the same size", () => {
    // A dock drag across these widths re-asks the server once, not 200 times.
    expect(octaveCeil(314)).toBe(512);
    expect(octaveCeil(500)).toBe(512);
    expect(octaveCeil(512)).toBe(512);
    expect(octaveCeil(513)).toBe(1024);
  });

  it("floors at 64 and passes through nothing", () => {
    // A card narrower than 64px still needs a stream a codec will accept.
    expect(octaveCeil(10)).toBe(64);
    expect(octaveCeil(0)).toBe(0);
    expect(octaveCeil(-5)).toBe(0);
  });
});

describe("drawHalved", () => {
  it("copies straight across when there is nothing to reduce", () => {
    const ctx = destination();
    const src = source(800, 600);
    drawHalved(ctx as unknown as CanvasRenderingContext2D, src, 800, 600, 0);
    expect(ctx.calls).toEqual([[src, 0, 0, 800, 600, 0, 0, 800, 600]]);
  });

  it("takes a single halving directly from the source", () => {
    const ctx = destination();
    const src = source(1920, 1080);
    drawHalved(ctx as unknown as CanvasRenderingContext2D, src, 1920, 1080, 1);
    expect(ctx.calls).toEqual([[src, 0, 0, 1920, 1080, 0, 0, 960, 540]]);
  });

  it("routes deeper reductions through scratch buffers", () => {
    const ctx = destination();
    const src = source(1920, 1080);
    drawHalved(ctx as unknown as CanvasRenderingContext2D, src, 1920, 1080, 3);

    // One write to the caller's canvas, and it is the *last* halving — the
    // earlier ones landed in scratch, so every source pixel is folded in
    // rather than skipped by a 8:1 bilinear tap.
    expect(ctx.calls).toHaveLength(1);
    const [image, sx, sy, sw, sh, dx, dy, dw, dh] = ctx.calls[0];
    expect(image).not.toBe(src);
    expect([sx, sy, sw, sh]).toEqual([0, 0, 480, 270]);
    expect([dx, dy, dw, dh]).toEqual([0, 0, 240, 135]);
    expect(dw).toBe(halve(1920, 3));
    expect(dh).toBe(halve(1080, 3));
  });

  it("reads back only the sub-rect each step wrote", () => {
    const ctx = destination();
    drawHalved(
      ctx as unknown as CanvasRenderingContext2D,
      source(1920, 1080),
      1920,
      1080,
      5,
    );
    // The scratch buffers are shared and grow-only, so the canvas backing a
    // step can be larger than the step. What keeps that sound is the explicit
    // source rect: it bounds the read to the region actually written.
    const [image, sx, sy, sw, sh] = ctx.calls[0];
    expect([sx, sy, sw, sh]).toEqual([0, 0, 120, 67]);
    expect((image as HTMLCanvasElement).width).toBeGreaterThanOrEqual(120);
  });

  it("reuses a scratch buffer across differently sized sources", () => {
    const big = destination();
    drawHalved(
      big as unknown as CanvasRenderingContext2D,
      source(1920, 1080),
      1920,
      1080,
      3,
    );
    const first = (big.calls[0][0] as HTMLCanvasElement).width;

    // A smaller thumbnail sharing the slot must not shrink it back — that
    // realloc is exactly the per-frame cost grow-only exists to avoid.
    const small = destination();
    drawHalved(
      small as unknown as CanvasRenderingContext2D,
      source(640, 480),
      640,
      480,
      3,
    );
    const second = (small.calls[0][0] as HTMLCanvasElement).width;
    expect(second).toBe(first);
    // ...and the smaller draw still reads only its own region.
    expect(small.calls[0].slice(1, 5)).toEqual([0, 0, 160, 120]);
  });
});
