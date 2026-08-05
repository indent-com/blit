import { describe, expect, it } from "vitest";
import { STATUS_NO_MERGE_BASE, STATUS_OTHER, statusText } from "../types";

describe("common status registry", () => {
  it("distinguishes backend errors from unknown statuses", () => {
    expect(statusText(STATUS_OTHER)).toBe("backend error");
    expect(statusText(200)).toBe("unknown status 200");
    // 13–127 are reserved: a newer server's status must not read as OTHER.
    expect(statusText(STATUS_NO_MERGE_BASE + 1)).toBe("unknown status 13");
  });
});
