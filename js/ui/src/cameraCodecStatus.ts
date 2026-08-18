/**
 * Why a camera format is not on offer.
 *
 * The media panel used to disable a format with one tooltip for every cause:
 * "either this browser cannot encode it or no connected desktop accepts it".
 * That sentence covers a browser with no encoder, a browser whose encoder
 * answered with the wrong chroma, a slow hardware session that produced no
 * frame, and a desktop that refuses the format — four different problems, one
 * of which is a blit bug and none of which the reader can tell apart. A camera
 * silently pinned to Motion JPEG is exactly when the difference matters.
 *
 * The probe already knows which it was (`cameraCodecProbeOutcomes`); this turns
 * that into the sentence the chip shows.
 */
import type { CameraCodecProbeOutcome } from "@blit-sh/core";

/** A `VIDEO_CODEC_*` mask's wire codecs: the bit index *is* the wire codec. */
function wireCodecsOf(bits: number): number[] {
  const codecs: number[] = [];
  for (let codec = 0; codec < 8; codec++) {
    if (bits & (1 << codec)) codecs.push(codec);
  }
  return codecs;
}

/** Ordered by how much they tell the reader: the first one present wins, so a
 *  chip covering both 4:2:0 and 4:4:4 reports the more specific complaint. */
const OUTCOME_PRIORITY: readonly CameraCodecProbeOutcome[] = [
  "no-test-frame",
  "wrong-format",
  "no-keyframe",
  "encoder-error",
  "config-unsupported",
  "no-webcodecs",
];

const OUTCOME_TEXT: Record<CameraCodecProbeOutcome, string> = {
  supported: "",
  "no-webcodecs": "This browser has no WebCodecs video encoder.",
  "no-test-frame":
    "Blit could not build a test frame in this browser, so no format could be checked.",
  "config-unsupported": "This browser cannot encode this format.",
  "encoder-error": "This browser's encoder failed on this format.",
  "no-keyframe":
    "This browser accepted the format but its encoder produced no frame in time — reload to try again.",
  "wrong-format":
    "This browser's encoder answered with a different chroma format, which the desktop could not decode as this one.",
};

/**
 * The tooltip for a camera-format chip, or `null` when the format is available.
 *
 * `browserCodecs` is the probe mask, `serverCodecs` what the connected desktops
 * accept, and `outcomes` the per-codec probe verdicts.
 */
export function cameraCodecUnavailableReason(
  bits: number,
  browserCodecs: number,
  serverCodecs: number,
  outcomes: ReadonlyMap<number, CameraCodecProbeOutcome>,
): string | null {
  if (browserCodecs & serverCodecs & bits) return null;
  const browserHas = Boolean(browserCodecs & bits);
  const serverHas = Boolean(serverCodecs & bits);
  if (browserHas && !serverHas) {
    return "No connected desktop accepts this format.";
  }
  const reported = wireCodecsOf(bits)
    .map((codec) => outcomes.get(codec))
    .filter((outcome): outcome is CameraCodecProbeOutcome =>
      Boolean(outcome && outcome !== "supported"),
    );
  const outcome = OUTCOME_PRIORITY.find((candidate) =>
    reported.includes(candidate),
  );
  const browserText = outcome
    ? OUTCOME_TEXT[outcome]
    : "This browser cannot encode this format.";
  return serverHas
    ? browserText
    : `${browserText} No connected desktop accepts it either.`;
}
