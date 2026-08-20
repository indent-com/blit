/**
 * The shape a surface thumbnail should hold.
 *
 * A dock card gives itself the window's aspect and `height: auto`, so this
 * ratio *is* the card's height. Which of a surface's two sizes it follows
 * therefore decides how often the card's box moves, and a card's box moving
 * costs a server-side encoder rebuild and a keyframe (`refreshScaledTarget`).
 *
 * The composite size (`width`/`height`) is the wrong one. It is the logical
 * size times whatever scale the *highest-DPI viewer* asked for — the server
 * mediates one surface across every viewer at the largest scale any of them
 * wants — floored onto the even 4:2:0 sampling grid. So it is off by up to a
 * pixel on each axis, and it moves when somebody else's display DPI does, for
 * a window that never changed shape. The logical size moves only when the app
 * resizes, which is the only thing a thumbnail should follow.
 */

/** Just the fields the ratio needs, so tests need no full surface. */
export interface SurfaceAspectDims {
  width: number;
  height: number;
  logicalWidth: number;
  logicalHeight: number;
}

/**
 * The `aspect-ratio` declaration for a card showing `surface`, as a style
 * fragment to spread — empty when no size is known yet.
 *
 * Empty rather than a guess: before the first `SURFACE_RESIZED` every dimension
 * is 0, and emitting `0 / 0` is a degenerate ratio the browser ignores in a way
 * that is harder to reason about than simply letting the placeholder canvas lay
 * the card out for that one frame.
 *
 * Falls back to the composite size for a server too old to report a logical
 * one, which is what this used before.
 */
export function cardAspectRatio(
  surface: SurfaceAspectDims,
): { "aspect-ratio": string } | Record<string, never> {
  const w = surface.logicalWidth > 0 ? surface.logicalWidth : surface.width;
  const h = surface.logicalHeight > 0 ? surface.logicalHeight : surface.height;
  if (!(w > 0) || !(h > 0)) return {};
  return { "aspect-ratio": `${w} / ${h}` };
}

/**
 * Per-surface signature of everything the thumbnail UI reads.
 *
 * `SurfaceStore` mutates a surface's dimensions in place, and Solid does not
 * track property access on plain objects, so a child reading
 * `props.surface.logicalWidth` only picks up a new value when its item gets a
 * fresh object reference. This signature is what decides that, so every field
 * a card renders has to appear in it — a dimension left out here is a card that
 * silently keeps a stale shape.
 */
export function surfaceCardSignature(
  surface: SurfaceAspectDims & {
    title: string;
    appId: string;
    origin?: {
      sandboxEngine: string;
      appId: string;
      instanceId: string;
    } | null;
  },
): string {
  return [
    surface.title,
    surface.appId,
    surface.origin?.sandboxEngine ?? "",
    surface.origin?.appId ?? "",
    surface.origin?.instanceId ?? "",
    `${surface.width}x${surface.height}`,
    `${surface.logicalWidth}x${surface.logicalHeight}`,
  ].join("\0");
}
