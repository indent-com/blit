import { describe, it, expect } from "vitest";
import { captureDelta, type CaptureState } from "../prediction";

/** A capture field with the caret at the end and nothing proposed. */
function typed(value: string, inputType = "insertText"): CaptureState {
  return {
    value,
    selectionStart: value.length,
    selectionEnd: value.length,
    composing: false,
    inputType,
  };
}

/** A capture field showing `committed` with `suggestion` proposed after it. */
function proposed(
  committed: string,
  suggestion: string,
  composing = false,
): CaptureState {
  return {
    value: committed + suggestion,
    selectionStart: committed.length,
    selectionEnd: committed.length + suggestion.length,
    composing,
    inputType: "insertText",
  };
}

describe("captureDelta", () => {
  it("forwards what was appended since the last call", () => {
    expect(captureDelta("git sta", typed("git stat"))).toEqual({
      deletes: 0,
      send: "t",
      mirror: "git stat",
      suggestion: "",
      restore: false,
    });
  });

  it("forwards a run appended in one event (a fast burst, an accepted word)", () => {
    const d = captureDelta("git ", typed("git status"));
    expect(d.send).toBe("status");
    expect(d.deletes).toBe(0);
  });

  it("never forwards the proposal, only the text before it", () => {
    const d = captureDelta("git st", proposed("git st", "atus"));
    expect(d.send).toBe("");
    expect(d.suggestion).toBe("atus");
    expect(d.mirror).toBe("git st");
  });

  it("forwards the proposal once accepting it makes it committed text", () => {
    // The host writes the whole suggestion into the field and collapses the
    // selection to the end: from here it is ordinary typed text.
    const d = captureDelta("git st", typed("git status"));
    expect(d.send).toBe("atus");
    expect(d.suggestion).toBe("");
  });

  it("turns a truncation into DELs", () => {
    const d = captureDelta(
      "git status",
      typed("git st", "deleteContentBackward"),
    );
    expect(d.deletes).toBe(4);
    expect(d.send).toBe("");
    expect(d.mirror).toBe("git st");
  });

  it("refuses a substitution over text already forwarded", () => {
    // Autocorrect rewriting "teh" to "the" must not reach the shell: those
    // bytes are already on the far side.
    const d = captureDelta("teh", {
      ...typed("the"),
      inputType: "insertReplacementText",
    });
    expect(d).toEqual({
      deletes: 0,
      send: "",
      mirror: null,
      suggestion: "",
      restore: true,
    });
  });

  it("refuses a shrink that does not claim to be a deletion", () => {
    const d = captureDelta("git status", typed("git st", "insertText"));
    expect(d.restore).toBe(true);
    expect(d.deletes).toBe(0);
  });

  it("holds a real composition, which has no proposed tail", () => {
    // Romaji resolving towards kana: forwarding each intermediate state would
    // put it on the shell's line only to delete it again.
    const d = captureDelta("", {
      value: "にほn",
      selectionStart: 3,
      selectionEnd: 3,
      composing: true,
      inputType: "insertCompositionText",
    });
    expect(d).toEqual({
      deletes: 0,
      send: "",
      mirror: null,
      suggestion: "",
      restore: false,
    });
  });

  it("treats a proposal delivered as marked text like any other proposal", () => {
    // If the host puts its prediction up as a composition, the selected tail
    // is what tells it apart from the case above.
    const d = captureDelta("git st", proposed("git st", "atus", true));
    expect(d.suggestion).toBe("atus");
    expect(d.send).toBe("");
    expect(d.mirror).toBe("git st");
  });

  it("keeps forwarding while a proposal is on screen", () => {
    // Typing does not stall because a prediction is showing: the committed
    // prefix grew, so the new character goes out.
    const d = captureDelta("git st", proposed("git sta", "tus", true));
    expect(d.send).toBe("a");
    expect(d.suggestion).toBe("tus");
  });

  it("is idempotent — a repeated event sends nothing twice", () => {
    const state = typed("git status");
    const first = captureDelta("git st", state);
    expect(first.send).toBe("atus");
    const second = captureDelta(first.mirror!, state);
    expect(second.send).toBe("");
    expect(second.deletes).toBe(0);
  });

  it("clamps a selection that runs past the value", () => {
    const d = captureDelta("ab", {
      value: "abc",
      selectionStart: 99,
      selectionEnd: 99,
      composing: false,
      inputType: "insertText",
    });
    expect(d.send).toBe("c");
    expect(d.suggestion).toBe("");
  });
});
