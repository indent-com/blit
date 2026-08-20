export type MarkdownDiagrams = Map<string, string>;

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function escapeAttr(value: string): string {
  return escapeHtml(value).replace(/"/g, "&quot;");
}

/** Render a fenced code token, extracting Mermaid for live SVG rendering. */
export function markdownCodeBlock(
  text: string,
  info: string | undefined,
  diagrams: MarkdownDiagrams,
  diagramPrefix: string,
): string {
  const language = (info ?? "").trim().split(/\s+/, 1)[0] ?? "";
  if (language.toLowerCase() === "mermaid") {
    const id = `${diagramPrefix}-${diagrams.size}`;
    diagrams.set(id, text);
    return `<div class="blit-mermaid" id="${id}"></div>`;
  }
  const cls = language ? ` class="language-${escapeAttr(language)}"` : "";
  return `<pre><code${cls}>${escapeHtml(text)}</code></pre>`;
}
