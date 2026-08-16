import { describe, expect, it } from "vitest";
import { cameraBitstreamMatchesCodec } from "../media";

/** Annex-B framing: each NAL preceded by a 4-byte start code. */
const stream = (...nals: number[][]): Uint8Array =>
  new Uint8Array(nals.flatMap((nal) => [0, 0, 0, 1, ...nal]));

/** SPS payload. For a non-high profile the SPS carries no chroma field at all
 *  and 4:2:0 is implied; for a high profile it is `ue(sps_id) ue(chroma)`,
 *  which is `1` followed by `010` (chroma 1) or `00100` (chroma 3). */
const sps = (profile: number, trailing: number[] = []) => [
  0x67,
  profile,
  0x00,
  0x28,
  ...trailing,
];
const pps = [0x68, 0xce, 0x38, 0x80];
const idr = [0x65, 0x88, 0x84, 0x00];

const BASELINE = 0x42;
const HIGH = 0x64;
const HIGH_444 = 0xf4;
/** `ue(0) ue(1)` = 1 010, padded. */
const CHROMA_420_BITS = [0xa0];
/** `ue(0) ue(3)` = 1 00100, padded. */
const CHROMA_444_BITS = [0x90];

describe("cameraBitstreamMatchesCodec", () => {
  it("accepts a High-profile 4:2:0 answer to a Baseline request", () => {
    // The regression this guards: blit asks for `avc1.4200…` and VideoToolbox
    // answers with High. Requiring the exact profile lost Safari H.264 and
    // dropped the whole camera to Motion JPEG, though the bitstream was
    // perfectly decodable and carried exactly the chroma promised on the wire.
    const bitstream = stream(sps(HIGH, CHROMA_420_BITS), pps, idr);
    expect(cameraBitstreamMatchesCodec(1, bitstream)).toBe(true);
  });

  it("accepts a Baseline 4:2:0 answer", () => {
    expect(
      cameraBitstreamMatchesCodec(1, stream(sps(BASELINE), pps, idr)),
    ).toBe(true);
  });

  it("still refuses 4:4:4 chroma for the 4:2:0 wire codec", () => {
    // Chroma is the one thing the wire codec actually promises the server,
    // which maps it to (H264, Cs420) — so this must stay strict.
    const bitstream = stream(sps(HIGH_444, CHROMA_444_BITS), pps, idr);
    expect(cameraBitstreamMatchesCodec(1, bitstream)).toBe(false);
  });

  it("requires 4:4:4 chroma for the 4:4:4 wire codec", () => {
    expect(
      cameraBitstreamMatchesCodec(
        3,
        stream(sps(HIGH_444, CHROMA_444_BITS), pps, idr),
      ),
    ).toBe(true);
    expect(
      cameraBitstreamMatchesCodec(
        3,
        stream(sps(HIGH, CHROMA_420_BITS), pps, idr),
      ),
    ).toBe(false);
  });

  it("refuses a keyframe missing its parameter sets", () => {
    // A stream with no PPS or no IDR is not something the server can start
    // decoding from, whatever its SPS claims.
    expect(cameraBitstreamMatchesCodec(1, stream(sps(BASELINE), idr))).toBe(
      false,
    );
    expect(cameraBitstreamMatchesCodec(1, stream(sps(BASELINE), pps))).toBe(
      false,
    );
  });
});
