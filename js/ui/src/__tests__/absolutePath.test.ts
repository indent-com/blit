import { describe, expect, it } from "vitest";
import { absolutePath } from "../ide/paths";

/**
 * What "Copy path" puts on the clipboard. Tree rows carry a root-relative,
 * always-`/`-separated path; what is useful to paste into a shell is the
 * absolute one, in that shell's own spelling.
 */
describe("absolutePath", () => {
  it("prefixes the synced root", () => {
    expect(absolutePath("/tmp/mercator", "src/proj.rs")).toBe(
      "/tmp/mercator/src/proj.rs",
    );
  });

  it("gives the relative path when the root is not known yet", () => {
    // Before the FS_SYNCED echo there is nothing better to offer, and a
    // relative path still pastes usefully next to a terminal in that root.
    expect(absolutePath(null, "src/proj.rs")).toBe("src/proj.rs");
  });

  it("does not double a separator", () => {
    expect(absolutePath("/tmp/mercator/", "a.rs")).toBe("/tmp/mercator/a.rs");
  });

  it("handles a root at the filesystem root", () => {
    expect(absolutePath("/", "etc/hosts")).toBe("/etc/hosts");
  });

  it("returns the root itself for an empty relative path", () => {
    expect(absolutePath("/tmp/mercator", "")).toBe("/tmp/mercator");
    expect(absolutePath("/", "")).toBe("/");
  });

  it("spells a Windows path the way its own shell wants", () => {
    // The wire says `src/proj.rs` whatever the host is; `C:\x/src/proj.rs`
    // is the one join that would come back to bite someone.
    expect(absolutePath("C:\\work\\proj", "src/proj.rs")).toBe(
      "C:\\work\\proj\\src\\proj.rs",
    );
    expect(absolutePath("C:\\work\\proj\\", "a.rs")).toBe(
      "C:\\work\\proj\\a.rs",
    );
  });

  it("keeps a posix root posix even if a name contains a backslash", () => {
    // A backslash is a legal character in a POSIX filename, so the separator
    // choice cannot be "does this string contain one".
    expect(absolutePath("/tmp/we\\ird", "a.rs")).toBe("/tmp/we\\ird/a.rs");
  });
});
