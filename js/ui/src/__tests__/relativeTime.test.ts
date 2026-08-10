import { describe, expect, it } from "vitest";
import { msUntilNextSecond, relativeTime } from "../ide/relativeTime";

describe("relativeTime", () => {
  it("counts every second before rolling over to minutes", () => {
    const committedAt = 1_000n;

    expect(relativeTime(committedAt, 1_002)).toBe("2s");
    expect(relativeTime(committedAt, 1_003)).toBe("3s");
    expect(relativeTime(committedAt, 1_004)).toBe("4s");
    expect(relativeTime(committedAt, 1_059)).toBe("59s");
    expect(relativeTime(committedAt, 1_060)).toBe("1m");
    expect(relativeTime(committedAt, 1_120)).toBe("2m");
  });

  it("clamps future commit times to zero", () => {
    expect(relativeTime(1_001n, 1_000)).toBe("0s");
  });
});

describe("msUntilNextSecond", () => {
  it("aligns refreshes to the next Unix-second boundary", () => {
    expect(msUntilNextSecond(1_234)).toBe(766);
    expect(msUntilNextSecond(1_999)).toBe(1);
    expect(msUntilNextSecond(2_000)).toBe(1_000);
  });
});
