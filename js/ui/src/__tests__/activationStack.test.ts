import { describe, expect, it } from "vitest";
import {
  ACTIVATION_STACK_LIMIT,
  popActivation,
  pushActivation,
} from "../activationStack";

/** Assignment strings are opaque to the stack; these are shaped like the real
 *  ones so the tests read the way the main view does. */
const S1 = "surface:conn-a:1";
const S2 = "surface:conn-a:2";
const S3 = "surface:conn-b:7";
const TERM = "session-abc";

const always = () => true;

describe("pushActivation", () => {
  it("records the occupant an activation covers", () => {
    expect(pushActivation([], TERM, S1)).toEqual([TERM]);
  });

  it("stacks successive activations oldest-first", () => {
    let stack = pushActivation([], TERM, S1);
    stack = pushActivation(stack, S1, S2);
    stack = pushActivation(stack, S2, S3);
    expect(stack).toEqual([TERM, S1, S2]);
  });

  it("records nothing when the slot was empty", () => {
    expect(pushActivation([], null, S1)).toEqual([]);
  });

  it("ignores an activation of what is already on screen", () => {
    expect(pushActivation([TERM], S1, S1)).toEqual([TERM]);
  });

  it("does not bury a second copy of a re-activated surface", () => {
    // S1 is already buried; activating it again from S2 must leave exactly one
    // entry for S1, or lowering S1 later would reveal it twice.
    const stack = pushActivation([TERM, S1], S2, S1);
    expect(stack).toEqual([TERM, S2]);
  });

  it("moves a re-displaced occupant to the top instead of aliasing it", () => {
    const stack = pushActivation([TERM, S1, S2], TERM, S3);
    expect(stack).toEqual([S1, S2, TERM]);
  });

  it("caps depth by dropping the oldest entries", () => {
    let stack: string[] = [];
    for (let i = 0; i < ACTIVATION_STACK_LIMIT + 5; i++) {
      stack = pushActivation(stack, `surface:conn-a:${i}`, "surface:conn-a:x");
    }
    expect(stack).toHaveLength(ACTIVATION_STACK_LIMIT);
    expect(stack[0]).toBe("surface:conn-a:5");
  });
});

describe("popActivation", () => {
  it("restores the entry beneath the activation", () => {
    expect(popActivation([TERM, S1], always)).toEqual({
      restore: S1,
      stack: [TERM],
    });
  });

  it("reports nothing to restore for an empty stack", () => {
    expect(popActivation([], always)).toEqual({ restore: null, stack: [] });
  });

  it("skips entries that died while they were buried", () => {
    // S1 and S2 closed off-screen; the terminal underneath is still there.
    const alive = (a: string) => a === TERM;
    expect(popActivation([TERM, S1, S2], alive)).toEqual({
      restore: TERM,
      stack: [],
    });
  });

  it("discards the whole stack when nothing survives", () => {
    expect(popActivation([S1, S2], () => false)).toEqual({
      restore: null,
      stack: [],
    });
  });

  it("leaves deeper entries in place for the next pop", () => {
    const first = popActivation([TERM, S1, S2], always);
    expect(first.restore).toBe(S2);
    expect(popActivation(first.stack, always)).toEqual({
      restore: S1,
      stack: [TERM],
    });
  });

  it("does not mutate the input stack", () => {
    const stack = [TERM, S1];
    popActivation(stack, always);
    expect(stack).toEqual([TERM, S1]);
  });
});
