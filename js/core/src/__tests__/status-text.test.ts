import { describe, expect, it } from "vitest";
import { FS_DONE_OTHER, fsDoneStatusText } from "../fs";
import { KV_STATUS_OTHER, kvStatusText } from "../kv";
import { LSP_STATUS_OTHER, lspStatusText } from "../lsp";
import { NET_STATUS_OTHER, netStatusText } from "../net";

describe("status text", () => {
  it("distinguishes backend errors from unknown statuses", () => {
    expect(fsDoneStatusText(FS_DONE_OTHER)).toBe("backend error");
    expect(fsDoneStatusText(200)).toBe("unknown status 200");
    expect(kvStatusText(KV_STATUS_OTHER)).toBe("backend error");
    expect(kvStatusText(200)).toBe("unknown status 200");
    expect(lspStatusText(LSP_STATUS_OTHER)).toBe("backend error");
    expect(lspStatusText(200)).toBe("unknown status 200");
    expect(netStatusText(NET_STATUS_OTHER)).toBe("backend error");
    expect(netStatusText(200)).toBe("unknown status 200");
  });
});
