import { describe, expect, it } from "vitest";
import { mergeStyle, ui } from "../theme";

/** These exist because the bug they guard against is invisible: Solid's
 *  compiler applies a `style` spread after the static properties written beside
 *  it, so `style={{ ...ui.btn, padding: 0 }}` renders ui.btn's padding while the
 *  source reads as an override. mergeStyle is the fix, so its precedence has to
 *  stay ordinary-JS obvious. */
describe("mergeStyle", () => {
  it("lets later arguments win, literal or not", () => {
    expect(mergeStyle(ui.btn, { padding: 0 }).padding).toBe(0);
    expect(mergeStyle(ui.btn, { padding: "4px" }).padding).toBe("4px");
    expect(mergeStyle(ui.btn, { opacity: 1 }).opacity).toBe(1);
    expect(mergeStyle(ui.btn, { "font-size": "inherit" })["font-size"]).toBe(
      "inherit",
    );
  });

  it("keeps base properties the caller did not override", () => {
    const merged = mergeStyle(ui.btn, { padding: 0 });
    expect(merged.background).toBe(ui.btn.background);
    expect(merged.cursor).toBe(ui.btn.cursor);
  });

  it("does not mutate the shared base", () => {
    const before = { ...ui.btn };
    mergeStyle(ui.btn, { padding: "99px", opacity: 0.1 });
    expect(ui.btn).toEqual(before);
  });

  it("applies arguments left to right across several bases", () => {
    expect(
      mergeStyle(
        { padding: "1px", color: "red" },
        { padding: "2px" },
        {
          padding: "3px",
        },
      ).padding,
    ).toBe("3px");
    expect(
      mergeStyle({ padding: "1px", color: "red" }, { padding: "2px" }).color,
    ).toBe("red");
  });

  it("skips falsy arguments so a conditional base can be inlined", () => {
    const off = false;
    expect(mergeStyle(ui.btn, off && { padding: "9px" }).padding).toBe(
      ui.btn.padding,
    );
    expect(mergeStyle(ui.btn, null, undefined, { padding: 0 }).padding).toBe(0);
  });
});
