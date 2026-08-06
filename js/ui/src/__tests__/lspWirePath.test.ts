import { describe, expect, it } from "vitest";
import { lspWirePath } from "../ide/paths";

/**
 * How an open editor names its file in every LSP message — the key its
 * diagnostics come back under, and the path its `LSP_BUFFER` overlay claims.
 * Mirrors the server's `wire_path` (`crates/lsp/src/text.rs`); the two ends
 * disagreeing means squiggles land on the wrong file, or on none.
 */
describe("lspWirePath", () => {
  it("strips the workspace root", () => {
    expect(lspWirePath("/src/blit", "/src/blit/crates/lsp/src/text.rs")).toBe(
      "crates/lsp/src/text.rs",
    );
  });

  it("tolerates a trailing slash on the root", () => {
    expect(lspWirePath("/src/blit/", "/src/blit/a.rs")).toBe("a.rs");
  });

  it("keeps a path outside the root absolute", () => {
    // Not a failure mode: a definition legitimately lands in the stdlib or a
    // registry checkout, and the server's `resolve_wire` takes it as-is.
    expect(
      lspWirePath("/src/blit", "/home/u/.cargo/registry/src/x/lib.rs"),
    ).toBe("/home/u/.cargo/registry/src/x/lib.rs");
  });

  it("does not treat a sibling with a shared prefix as a child", () => {
    // "/a/bc" is not under "/a/b". Slicing by the root's length alone yields
    // "c" — a path that resolves to a different file that may well exist.
    expect(lspWirePath("/a/b", "/a/bc/main.rs")).toBe("/a/bc/main.rs");
  });

  it("does not fall back to the bare filename when the root does not match", () => {
    // The regression this guards: any path not spelled as a literal prefix of
    // the canonical root — a symlinked checkout — used to collapse to
    // "main.rs" and address whatever sat at the workspace root.
    expect(lspWirePath("/real/proj", "/link/proj/src/main.rs")).toBe(
      "/link/proj/src/main.rs",
    );
  });

  it("gives the path unchanged before a root is known", () => {
    expect(lspWirePath(null, "/src/blit/a.rs")).toBe("/src/blit/a.rs");
  });

  it("handles a Windows root", () => {
    expect(lspWirePath("C:\\src\\blit", "C:\\src\\blit\\a.rs")).toBe("a.rs");
  });

  it("resolves the root itself to the empty path", () => {
    expect(lspWirePath("/src/blit", "/src/blit")).toBe("");
  });
});
