import { describe, expect, it } from "vitest";
import { settleAttention } from "../surfaceAttention";

/** Assignment strings are opaque here; these are shaped like the real ones so
 *  the tests read the way the workspace does. */
const S1 = "surface:conn-a:1";
const S2 = "surface:conn-b:7";

describe("settleAttention", () => {
  const alive = () => true;

  it("keeps an unanswered request, by identity", () => {
    const pending: ReadonlySet<string> = new Set([S1]);
    expect(settleAttention(pending, null, alive)).toBe(pending);
  });

  it("does not time out — that is the point of it", () => {
    // No `now` anywhere in the signature: only being looked at clears it.
    let pending: ReadonlySet<string> = new Set([S1]);
    for (let i = 0; i < 100; i++)
      pending = settleAttention(pending, null, alive);
    expect([...pending]).toEqual([S1]);
  });

  it("clears the surface the viewer is looking at", () => {
    expect(settleAttention(new Set([S1, S2]), S1, alive)).toEqual(
      new Set([S2]),
    );
  });

  it("clears a surface that went away", () => {
    const live = (a: string) => a !== S2;
    expect(settleAttention(new Set([S1, S2]), null, live)).toEqual(
      new Set([S1]),
    );
  });

  it("ignores an on-top surface that never asked", () => {
    const pending: ReadonlySet<string> = new Set([S1]);
    expect(settleAttention(pending, S2, alive)).toBe(pending);
  });

  it("is identity on an empty set", () => {
    const none: ReadonlySet<string> = new Set();
    expect(settleAttention(none, S1, alive)).toBe(none);
  });
});
