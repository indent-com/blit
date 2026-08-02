import { describe, expect, it } from "vitest";
import type { PreviewTarget } from "@blit-sh/core";
import { injectIntoHtml, shimTag } from "../inject";

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

function streamOf(...chunks: Uint8Array<ArrayBuffer>[]) {
  return new ReadableStream<Uint8Array<ArrayBuffer>>({
    start(controller) {
      for (const c of chunks) controller.enqueue(c);
      controller.close();
    },
  });
}

async function drain(stream: ReadableStream<Uint8Array>): Promise<string> {
  const reader = stream.getReader();
  const parts: Uint8Array[] = [];
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    parts.push(value);
  }
  const total = parts.reduce((n, p) => n + p.length, 0);
  const merged = new Uint8Array(total);
  let at = 0;
  for (const p of parts) {
    merged.set(p, at);
    at += p.length;
  }
  return new TextDecoder().decode(merged);
}

describe("injectIntoHtml", () => {
  const encode = (s: string) => new TextEncoder().encode(s);

  it("inserts right after <head>", async () => {
    const html =
      "<!DOCTYPE html><html><head><title>x</title></head><body></body></html>";
    const out = await drain(injectIntoHtml(streamOf(encode(html)), target, ""));
    expect(out).toMatch(/<head><script>/);
    expect(out.endsWith("</body></html>")).toBe(true);
  });

  // Regression: the insertion index comes from a decoded string but is used
  // as a byte offset. blit.sh opens with an HTML comment containing em dashes
  // (3 bytes, 1 char each), and the drift landed the shim inside `<head>`
  // itself — `<he<script>…` — so the whole shim rendered as page text.
  it("stays on tag boundaries when multi-byte chars precede <head>", async () => {
    const html =
      '<!DOCTYPE html><!-- the arc — the claim — the acts --><html lang="en"><head><meta charset="utf-8"></head><body>ok</body></html>';
    const out = await drain(injectIntoHtml(streamOf(encode(html)), target, ""));
    expect(out).toContain("<head><script>");
    expect(out).not.toContain("<he<script>");
    // The document around the shim is byte-identical to the input.
    expect(out.replace(/<script>[\s\S]*<\/script>/, "")).toBe(html);
  });

  it("survives a chunk boundary inside a multi-byte char", async () => {
    const bytes = encode(
      "<!-- — —— — --><html><head></head><body></body></html>",
    );
    const cut = bytes.indexOf(0xe2) + 1; // split an em dash across chunks
    const out = await drain(
      injectIntoHtml(streamOf(bytes.slice(0, cut), bytes.slice(cut)), target, ""),
    );
    expect(out).toContain("<head><script>");
    expect(out.replace(/<script>[\s\S]*<\/script>/, "")).toBe(
      new TextDecoder().decode(bytes),
    );
  });
});
