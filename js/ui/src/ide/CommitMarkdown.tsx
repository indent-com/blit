import { createEffect, createSignal, onCleanup, type JSX } from "solid-js";
import { Marked } from "marked";
import { renderMermaid } from "../mermaid";
import type { Theme } from "../theme";
import { markdownCodeBlock, type MarkdownDiagrams } from "./markdownCode";
import "./commitMarkdown.css";

const SAFE_PROTOCOL = /^(https?|mailto)$/i;
const SAFE_IMAGE_PROTOCOL = /^https?$/i;

/** Keep links useful without letting untrusted commit text execute a URL. */
export function safeMarkdownUrl(href: string): string {
  const colon = href.indexOf(":");
  if (colon === -1) return href;

  const firstRelativeDelimiter = ["/", "?", "#"]
    .map((delimiter) => href.indexOf(delimiter))
    .filter((index) => index !== -1)
    .reduce((first, index) => Math.min(first, index), Infinity);
  if (colon > firstRelativeDelimiter) return href;

  return SAFE_PROTOCOL.test(href.slice(0, colon)) ? href : "";
}

/** Images may be remote or relative, but never executable/data URLs. */
export function safeMarkdownImageUrl(src: string): string {
  const colon = src.indexOf(":");
  if (colon === -1) return src;

  const firstRelativeDelimiter = ["/", "?", "#"]
    .map((delimiter) => src.indexOf(delimiter))
    .filter((index) => index !== -1)
    .reduce((first, index) => Math.min(first, index), Infinity);
  if (colon > firstRelativeDelimiter) return src;

  return SAFE_IMAGE_PROTOCOL.test(src.slice(0, colon)) ? src : "";
}

let markdownSerial = 0;

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function escapeAttr(value: string): string {
  return escapeHtml(value).replace(/"/g, "&quot;");
}

/**
 * `marked` rather than a remark stack: remark reaches micromark, which reaches
 * `debug` — a CJS package vite serves raw over `/@fs/`, where it fails as ESM
 * ("does not provide an export named 'default'"). marked is one
 * dependency-free ESM package, so that chain is gone.
 *
 * The cost is that marked emits an HTML string rather than components, so what
 * `solid-markdown` used to guarantee (`skipHtml`, `disallowedElements`,
 * `transformLinkUri`) is enforced here instead: raw HTML is dropped, code and
 * attributes are escaped, and every link/image URL is checked. Commit text is
 * repo-authored, not trusted.
 */
function buildMarked(
  theme: Theme,
  diagrams: MarkdownDiagrams,
  diagramPrefix: string,
): Marked {
  const marked = new Marked({ gfm: true, breaks: false, async: false });
  marked.use({
    renderer: {
      html(): string {
        return "";
      },
      image({ href, title, text }): string {
        const safe = safeMarkdownImageUrl(href ?? "");
        if (!safe) return escapeHtml(text ?? "");
        const attrs = title ? ` title="${escapeAttr(title)}"` : "";
        return `<img src="${escapeAttr(safe)}" alt="${escapeAttr(text ?? "")}" loading="lazy" decoding="async" referrerpolicy="no-referrer"${attrs}>`;
      },
      link({ href, title, tokens }): string {
        const safe = safeMarkdownUrl(href ?? "");
        const text = this.parser.parseInline(tokens);
        if (!safe) return text;
        const attrs = title ? ` title="${escapeAttr(title)}"` : "";
        return `<a href="${escapeAttr(safe)}" target="_blank" rel="noopener noreferrer" style="color:${escapeAttr(theme.accent)}"${attrs}>${text}</a>`;
      },
      code({ text, lang }): string {
        return markdownCodeBlock(text, lang, diagrams, diagramPrefix);
      },
    },
  });
  return marked;
}

export function CommitMarkdown(props: {
  children: string;
  theme: Theme;
  variant: "subject" | "body";
}): JSX.Element {
  const [html, setHtml] = createSignal("");
  let host: HTMLDivElement | undefined;
  const diagramPrefix = `blit-commit-mermaid-${++markdownSerial}`;

  createEffect(() => {
    const diagrams: MarkdownDiagrams = new Map();
    const marked = buildMarked(props.theme, diagrams, diagramPrefix);
    const rendered = marked.parse(props.children ?? "", { async: false });
    setHtml(typeof rendered === "string" ? rendered : "");

    if (diagrams.size === 0) return;
    let cancelled = false;
    onCleanup(() => {
      cancelled = true;
    });
    queueMicrotask(() => {
      void (async () => {
        for (const [id, source] of diagrams) {
          if (cancelled) return;
          const target = host?.querySelector<HTMLElement>(`#${id}`);
          if (!target) continue;
          try {
            const svg = await renderMermaid(`${id}-svg`, source, props.theme);
            if (!cancelled) target.innerHTML = svg;
          } catch (err) {
            if (cancelled) return;
            // A malformed diagram shows its source rather than vanishing: the
            // author is the one who can fix it.
            target.textContent = source;
            target.setAttribute(
              "title",
              err instanceof Error ? err.message : String(err),
            );
          }
        }
      })();
    });
  });

  return (
    <div
      ref={(el) => (host = el)}
      class={`blit-commit-markdown blit-commit-markdown--${props.variant}`}
      // Safe by construction above: raw HTML and images dropped, hrefs
      // filtered, code escaped.
      innerHTML={html()}
    />
  );
}
