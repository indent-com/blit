/**
 * Wire encoding for the two surface-video axes, and the words the panel puts
 * on them.
 *
 * Both `bandwidth` and `speed` travel as one byte in C2S_SURFACE_SUBSCRIBE,
 * and the byte is not a scale — it is a small tagged union:
 *
 *   0        server default
 *   1–4      named presets
 *   5–9      reserved; the server reads these as the default
 *   10–255   a raw value
 *
 * The two axes disagree about which way "better" runs, which is the whole
 * reason this module exists:
 *
 * - **bandwidth** presets climb (1 = Low, quantizer 180 … 4 = Ultra, quantizer
 *   1) while its raw range is an AV1 quantizer, where *higher is worse*. A
 *   slider over 10–255 therefore runs backwards from the preset row above it.
 * - **speed** agrees in both (1 = Slow … 4 = Realtime; raw 10 = slowest,
 *   255 = fastest), so it needs no mirroring.
 *
 * See `SurfaceBandwidth::from_wire` / `SurfaceSpeed::from_wire` in
 * crates/server/src/surface_encoder.rs for the other end.
 */

/** Lowest byte that means "a raw value" rather than a preset. */
export const CUSTOM_WIRE_MIN = 10;
export const CUSTOM_WIRE_MAX = 255;

/** Whether a stored byte is a raw custom value rather than a preset. */
export function isCustomWire(value: number): boolean {
  return value >= CUSTOM_WIRE_MIN && value <= CUSTOM_WIRE_MAX;
}

/**
 * Mirror a byte about the custom range, so a slider can run low-to-high while
 * the quantizer underneath still descends. Its own inverse.
 *
 * Deliberately *not* extended below `CUSTOM_WIRE_MIN`: bytes 1–4 are the
 * presets and 5–9 are reserved, so a slider that reached below 10 would send
 * the *worst* preset from its best-looking end. The custom range genuinely
 * cannot express the top preset's quantizer of 1 — hence "Near best".
 */
export function flipWire(value: number): number {
  return CUSTOM_WIRE_MIN + CUSTOM_WIRE_MAX - value;
}

/**
 * How much detail a quantizer keeps, in words.
 *
 * Thresholds are the named presets' own quantizers (Low 180, Medium 120,
 * High 80, Ultra 1), so the word a custom value gets agrees with the chip it
 * sits between.
 */
export function detailWord(quantizer: number): string {
  if (quantizer <= 10) return "highest";
  if (quantizer <= 40) return "very high";
  if (quantizer <= 80) return "high";
  if (quantizer <= 120) return "medium";
  if (quantizer <= 180) return "low";
  return "lowest";
}

/**
 * How much effort an encoder speed spends, in words.
 *
 * The server folds 10–255 onto a 0–10 effort level on which the named presets
 * land at 4/6/8/10, so these boundaries are the presets' own.
 */
export function effortWord(speed: number): string {
  if (speed <= 59) return "most";
  if (speed <= 108) return "more";
  if (speed <= 157) return "medium";
  if (speed <= 206) return "less";
  return "least";
}
