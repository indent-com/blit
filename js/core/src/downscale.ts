/**
 * Box-filtered minification for canvases the browser would otherwise shrink
 * on its own.
 *
 * A canvas whose backing store is bigger than its CSS box is resampled by the
 * compositor with one bilinear tap: four source pixels out of every N², which
 * past 2:1 throws most of the image away and reads as nearest-neighbour —
 * dropped scanlines in a video surface, disintegrating stems in a glyph.
 * Nothing in CSS fixes it. There is no mip chain behind a canvas, and every
 * `image-rendering` value still samples at most four texels.
 *
 * Halving in Canvas2D does fix it: at exactly 2:1 a bilinear tap *is* the 2×2
 * box average, so a chain of halves visits every source pixel. Reductions are
 * therefore quantised to powers of two, and the remainder — always under 2:1,
 * where one tap is the right filter — is left to CSS. Quantising also keeps
 * the picture stable while a pane is dragged: the chain only changes at
 * octave boundaries, not on every pixel of the drag.
 */

/** Hard stop on the chain. 2^6 covers any real box; beyond it the source is
 *  degenerate and the extra passes buy nothing. */
const MAX_HALVINGS = 6;

/**
 * How many halvings bring an `sw`×`sh` image down to at most `bw`×`bh`,
 * measured the way `object-fit: contain` measures it. 0 when it already fits,
 * so callers can treat "no reduction" as the ordinary 1:1 path.
 */
export function halvings(
  sw: number,
  sh: number,
  bw: number,
  bh: number,
): number {
  if (!(sw > 0) || !(sh > 0) || !(bw > 0) || !(bh > 0)) return 0;
  const reduction = Math.max(sw / bw, sh / bh);
  if (!(reduction > 1)) return 0;
  return Math.min(MAX_HALVINGS, Math.floor(Math.log2(reduction)));
}

/** The extent an axis lands on after `n` halvings. Size the destination with
 *  this so it matches what {@link drawHalved} writes. */
export function halve(extent: number, n: number): number {
  return Math.max(1, extent >> n);
}

/**
 * Round an extent up to the next power of two, floored at 64.
 *
 * For asking someone else — a server encoder — for a size, where the cost of
 * changing your mind is far higher than the cost of overshooting. Snapping to
 * octaves means a drag re-asks a couple of times instead of on every pixel,
 * and the ≤2:1 overshoot lands exactly where {@link drawHalved} and a single
 * CSS tap clean it up for free.
 */
export function octaveCeil(extent: number): number {
  if (!(extent > 0)) return 0;
  return Math.max(64, 2 ** Math.ceil(Math.log2(extent)));
}

/**
 * A ResizeObserver entry's content box in the device pixels the compositor
 * will rasterise it at.  Returns null for a zero-sized (detached,
 * display:none) box.
 *
 * Deliberately not `devicePixelContentBoxSize`, which looks like the exact
 * answer and is not always: under a headless browser's device-scale-factor
 * emulation it reports the CSS box unscaled, and believing it costs a whole
 * extra octave of reduction — a thumbnail blurrier than the one this module
 * exists to fix.  `devicePixelRatio` is the ratio the layer is actually
 * rasterised with, and octave quantisation makes the sub-pixel precision
 * `devicePixelContentBoxSize` would buy irrelevant.
 */
export function devicePixelBox(
  entry: ResizeObserverEntry,
): { width: number; height: number } | null {
  const dpr = (globalThis.devicePixelRatio ?? 1) || 1;
  const width = Math.round(entry.contentRect.width * dpr);
  const height = Math.round(entry.contentRect.height * dpr);
  return width > 0 && height > 0 ? { width, height } : null;
}

/** Ping-pong scratch buffers for the intermediate steps of the chain. */
const scratches: (CanvasRenderingContext2D | null)[] = [];

function scratch(
  slot: number,
  w: number,
  h: number,
): CanvasRenderingContext2D | null {
  let ctx = scratches[slot];
  if (ctx === undefined) {
    ctx =
      typeof document === "undefined"
        ? null
        : document.createElement("canvas").getContext("2d");
    if (ctx) ctx.imageSmoothingEnabled = true;
    scratches[slot] = ctx;
  }
  if (!ctx) return null;
  const canvas = ctx.canvas;
  // Grow-only, like the shared renderer canvas (gl-renderer.ts): assigning
  // width reallocates and clears, and these slots are shared by every
  // thumbnail on the page, so sizing them exactly would make two cards of
  // different sizes realloc each other's buffer on every frame.
  //
  // Safe because each step only ever draws the sub-rect it just wrote, and at
  // 2:1 the sample footprint stays inside it: destination pixel j reads source
  // [2j, 2j+2), so the last tap of a w-wide region lands at w-0.5. Nothing
  // reaches the stale pixels beyond it.
  if (canvas.width < w) canvas.width = w;
  if (canvas.height < h) canvas.height = h;
  return ctx;
}

/**
 * Draw `src` into `ctx` at the origin, reduced by `2**n`, one halving at a
 * time. The destination must already be {@link halve}d to match.
 *
 * `ctx` must be at identity — the chain draws in destination pixels.
 */
export function drawHalved(
  ctx: CanvasRenderingContext2D,
  src: CanvasImageSource,
  sw: number,
  sh: number,
  n: number,
): void {
  const dw = halve(sw, n);
  const dh = halve(sh, n);
  let cur = src;
  let cw = sw;
  let ch = sh;
  // All but the last step land in a scratch buffer; the last one lands in the
  // destination, so the caller's canvas is written exactly once.
  for (let i = 0; i < n - 1; i++) {
    const nw = Math.max(1, cw >> 1);
    const nh = Math.max(1, ch >> 1);
    const step = scratch(i & 1, nw, nh);
    // No 2D context available: give up on the chain and take the single tap.
    // Worse looking, but the geometry still comes out right.
    if (!step) break;
    step.drawImage(cur, 0, 0, cw, ch, 0, 0, nw, nh);
    cur = step.canvas;
    cw = nw;
    ch = nh;
  }
  ctx.drawImage(cur, 0, 0, cw, ch, 0, 0, dw, dh);
}
