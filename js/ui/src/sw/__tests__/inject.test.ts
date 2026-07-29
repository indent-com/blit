import { describe, expect, it } from "vitest";
import type { PreviewTarget } from "@blit-sh/core";
import { shimTag } from "../inject";

const target: PreviewTarget = {
  dest: "local",
  scheme: "http",
  host: "localhost",
  port: 3000,
};

const decode = (bytes: Uint8Array) => new TextDecoder().decode(bytes);

describe("shimTag", () => {
  // The shim interpolates two page-influenced values into an inline
  // <script>: the cookie jar (from the dev server's Set-Cookie and from the
  // page's own document.cookie writes) and the target host. A value holding
  // `</script>` would close the element early and leave the rest of the
  // shim to be parsed as markup.
  it("cannot be closed early by a cookie containing a script end tag", () => {
    const html = decode(shimTag(target, "evil=</script><img src=x onerror=1>"));
    const closes = html.match(/<\/script>/gi) ?? [];
    expect(closes).toHaveLength(1);
    expect(html.endsWith("</script>")).toBe(true);
    // The value survives, escaped — this is not sanitising the cookie away.
    expect(html).toContain("\\u003c/script>");
  });

  it("escapes every < it embeds, whatever the case or spacing", () => {
    for (const value of [
      "</script>",
      "</SCRIPT>",
      "</script >",
      "</ script>",
      "<script>alert(1)</script>",
    ]) {
      const html = decode(shimTag(target, `c=${value}`));
      expect((html.match(/<\/script>/gi) ?? []).length, value).toBe(1);
    }
  });

  it("escapes a host carrying a script end tag too", () => {
    const html = decode(
      shimTag({ ...target, host: "a</script><b" }, "plain=1"),
    );
    expect((html.match(/<\/script>/gi) ?? []).length).toBe(1);
  });

  it("still produces a runnable script for ordinary input", () => {
    const html = decode(shimTag(target, "sid=abc; theme=dark"));
    expect(html.startsWith("<script>")).toBe(true);
    // The JSON stays valid JS: `\u003c` only appears where a `<` was.
    expect(html).toContain('"host":"localhost"');
    expect(html).toContain("sid=abc; theme=dark");
  });
});
