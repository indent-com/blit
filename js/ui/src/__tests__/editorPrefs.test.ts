import { afterEach, describe, expect, it } from "vitest";
import { EDITOR_WRAP_KEY, writeStorage } from "../storage";
import { lineWrap, setLineWrap } from "../ide/editorPrefs";

describe("shared editor preferences", () => {
  afterEach(() => {
    setLineWrap(true);
    localStorage.removeItem(EDITOR_WRAP_KEY);
  });

  it("reacts to a config value published by another frontend", () => {
    setLineWrap(true);

    writeStorage(EDITOR_WRAP_KEY, "0");

    expect(lineWrap()).toBe(false);
  });
});
