import { describe, expect, it } from "vitest";
import { markdownCodeBlock } from "../ide/markdownCode";

describe("markdownCodeBlock", () => {
  it("extracts Mermaid fences into unique placeholders", () => {
    const diagrams = new Map<string, string>();
    expect(
      markdownCodeBlock("flowchart LR\nA --> B", "mermaid", diagrams, "doc"),
    ).toBe('<div class="blit-mermaid" id="doc-0"></div>');
    expect(markdownCodeBlock("A --> C", "Mermaid title", diagrams, "doc")).toBe(
      '<div class="blit-mermaid" id="doc-1"></div>',
    );
    expect([...diagrams.values()]).toEqual([
      "flowchart LR\nA --> B",
      "A --> C",
    ]);
  });

  it("keeps ordinary code escaped and out of the diagram queue", () => {
    const diagrams = new Map<string, string>();
    expect(markdownCodeBlock("<script>&", "html", diagrams, "doc")).toBe(
      '<pre><code class="language-html">&lt;script&gt;&amp;</code></pre>',
    );
    expect(diagrams.size).toBe(0);
  });
});
