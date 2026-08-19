import { describe, expect, it } from "vitest";
import {
  ATTENTION_MS,
  armAttention,
  expireAttention,
  type Attention,
} from "../surfaceAttention";

/** Assignment strings are opaque here; these are shaped like the real ones so
 *  the tests read the way the workspace does. */
const S1 = "surface:conn-a:1";
const S2 = "surface:conn-b:7";

const empty: Attention = new Map();

describe("armAttention", () => {
  it("lights a surface until the window closes", () => {
    expect([...armAttention(empty, S1, 1000)]).toEqual([
      [S1, 1000 + ATTENTION_MS],
    ]);
  });

  it("lights surfaces independently", () => {
    const both = armAttention(armAttention(empty, S1, 1000), S2, 1200);
    expect([...both.keys()]).toEqual([S1, S2]);
  });

  it("leaves an open window alone, by identity", () => {
    // The retransmission case: the highlight the repeat is asking for is
    // already on screen, and re-arming would restart its animation.
    const lit = armAttention(empty, S1, 1000);
    expect(armAttention(lit, S1, 1000 + ATTENTION_MS - 1)).toBe(lit);
  });

  it("does not let a chatty client hold the window open", () => {
    // A request every 100ms: the first one owns the window and it still closes
    // on time, rather than being pushed out by each repeat.
    let a = armAttention(empty, S1, 0);
    for (let now = 100; now < ATTENTION_MS; now += 100) {
      a = armAttention(a, S1, now);
    }
    expect(a.get(S1)).toBe(ATTENTION_MS);
  });

  it("pulses once per window however long a client keeps asking", () => {
    // Ten requests a second for ten seconds is one pulse per window, not one
    // per request — the difference between a card that blinks and one that
    // strobes.
    let a = empty;
    let pulses = 0;
    for (let now = 0; now < 10_000; now += 100) {
      const next = armAttention(a, S1, now);
      if (next !== a) pulses++;
      a = expireAttention(next, now);
    }
    expect(pulses).toBe(Math.ceil(10_000 / ATTENTION_MS));
  });

  it("re-lights once the window has closed", () => {
    const lit = armAttention(empty, S1, 1000);
    const again = armAttention(lit, S1, 1000 + ATTENTION_MS);
    expect(again).not.toBe(lit);
    expect(again.get(S1)).toBe(1000 + ATTENTION_MS * 2);
  });
});

describe("expireAttention", () => {
  it("drops a closed window", () => {
    const lit = armAttention(empty, S1, 1000);
    expect(expireAttention(lit, 1000 + ATTENTION_MS).size).toBe(0);
  });

  it("keeps an open one, by identity", () => {
    const lit = armAttention(empty, S1, 1000);
    expect(expireAttention(lit, 1000 + ATTENTION_MS - 1)).toBe(lit);
  });

  it("expires each window on its own schedule", () => {
    const both = armAttention(armAttention(empty, S1, 0), S2, 500);
    const swept = expireAttention(both, ATTENTION_MS);
    expect([...swept.keys()]).toEqual([S2]);
  });

  it("is identity on an empty map", () => {
    expect(expireAttention(empty, 9999)).toBe(empty);
  });
});
