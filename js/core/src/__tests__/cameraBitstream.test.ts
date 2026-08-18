import { describe, expect, it } from "vitest";
import { cameraBitstreamMatchesCodec, cameraKeyframeForWire } from "../media";

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

const hexBytes = (hex: string): Uint8Array =>
  new Uint8Array(
    (hex.match(/../g) ?? []).map((byte) => Number.parseInt(byte, 16)),
  );

/**
 * Keyframe heads (SPS, PPS, and the start of the IDR NAL) taken from a real
 * browser WebCodecs H.264 encoder — Chromium 151, `avc: {format: "annexb"}` —
 * asked for High and for Main at 1280x720.
 *
 * Those are the two answers macOS VideoToolbox gives blit's `avc1.4200…`
 * Baseline request, which is the whole reason this file exists. Hand-written
 * parameter sets cannot show that a real encoder's SPS parses the way the rule
 * assumes: High spells its `chroma_format_idc` out (1, here) while Main leaves
 * 4:2:0 implied, and both must read as the 4:2:0 wire codec.
 */
const BROWSER_HIGH_420 = hexBytes(
  "0000000167640c1fac18d00a00b74d41818181e1108d400000000168ce3c800000000165b8000411fffff8",
);
const BROWSER_MAIN_420 = hexBytes(
  "00000001674d4c1f8c6805005ba6a0c0c0c0f08846a00000000168ce3c800000000165b8000411fffff8",
);

/**
 * Real AV1 keyframe heads from the same encoder, profile 0 (4:2:0) and 1
 * (4:4:4). Both open with `12 00` — a temporal delimiter OBU carrying no
 * payload, which is what every AV1 encoder emits at the start of a temporal
 * unit, and what the OBU walk has to step over to reach the sequence header.
 */
const BROWSER_AV1_420 = hexBytes(
  "12000a0e0000002d4cffb3c01a20c0c0c08032cc04106861002cb2cb1492400000008001b4b4b317",
);
const BROWSER_AV1_444 = hexBytes(
  "12000a0d2000002d4cffb3c01a41818184329806106861002cb2cb1492400000004b02b4b4b3174e",
);
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

describe("cameraKeyframeForWire", () => {
  it("sends a High-profile 4:2:0 keyframe answering a Baseline request", () => {
    // The live-stream half of the same regression: the probe accepted this and
    // the panel offered H.264, then every session's first keyframe was rejected
    // here and took the lease down, leaving macOS on Motion JPEG anyway.
    const data = stream(sps(HIGH, CHROMA_420_BITS), pps, idr);
    const decision = cameraKeyframeForWire(1, data, null);
    expect(decision.action).toBe("send");
    if (decision.action !== "send") return;
    expect(decision.data).toBe(data);
    // The parameter sets are worth remembering for keyframes that omit them.
    expect(decision.header).not.toBeNull();
  });

  it("rejects a keyframe whose chroma is not what the wire codec promised", () => {
    const decision = cameraKeyframeForWire(
      1,
      stream(sps(HIGH_444, CHROMA_444_BITS), pps, idr),
      null,
    );
    expect(decision.action).toBe("reject");
  });

  it("prepends the remembered parameter sets to a bare keyframe", () => {
    const header = stream(sps(HIGH, CHROMA_420_BITS), pps);
    const bare = stream(idr);
    const decision = cameraKeyframeForWire(1, bare, header);
    expect(decision.action).toBe("send");
    if (decision.action !== "send") return;
    expect(Array.from(decision.data)).toEqual([
      ...Array.from(header),
      ...Array.from(bare),
    ]);
    // Nothing new to remember — the prefix came from the cache.
    expect(decision.header).toBeNull();
  });

  it("drops a bare keyframe when nothing is remembered yet", () => {
    // Sending it would hand the server a picture it cannot start from.
    expect(cameraKeyframeForWire(1, stream(idr), null).action).toBe("drop");
  });

  it("accepts a real AV1 keyframe behind its temporal delimiter", () => {
    // The zero-length temporal delimiter used to end the OBU walk, so blit read
    // its own AV1 streams as profile-less and refused every one of them.
    expect(cameraBitstreamMatchesCodec(2, BROWSER_AV1_420)).toBe(true);
    expect(cameraBitstreamMatchesCodec(4, BROWSER_AV1_444)).toBe(true);
    // The two profiles are still told apart.
    expect(cameraBitstreamMatchesCodec(4, BROWSER_AV1_420)).toBe(false);
    expect(cameraBitstreamMatchesCodec(2, BROWSER_AV1_444)).toBe(false);
    // And the sequence header is what gets remembered as the wire prefix.
    const decision = cameraKeyframeForWire(2, BROWSER_AV1_420, null);
    expect(decision.action).toBe("send");
    if (decision.action !== "send") return;
    expect(decision.header?.[0]).toBe(0x0a);
  });

  it("sends what a real browser encoder answers a Baseline request with", () => {
    for (const bitstream of [BROWSER_HIGH_420, BROWSER_MAIN_420]) {
      expect(cameraBitstreamMatchesCodec(1, bitstream)).toBe(true);
      expect(cameraKeyframeForWire(1, bitstream, null).action).toBe("send");
      // The same bitstream is not 4:4:4, whatever profile it announces.
      expect(cameraKeyframeForWire(3, bitstream, null).action).toBe("reject");
    }
  });
});
