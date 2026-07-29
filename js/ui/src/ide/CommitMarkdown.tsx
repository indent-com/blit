import { createEffect, createSignal, onCleanup, type JSX } from "solid-js";
import { Marked } from "marked";
import type { Theme } from "../theme";
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

/** Mermaid sources pulled out before parsing, keyed by placeholder id. */
type Diagrams = Map<string, string>;

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
function buildMarked(theme: Theme, diagrams: Diagrams): Marked {
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
        // A mermaid fence becomes a placeholder: mermaid needs a live element,
        // so the diagram is rendered once this HTML is in the DOM.
        if ((lang ?? "").trim().toLowerCase() === "mermaid") {
          const id = `blit-mermaid-${diagrams.size}`;
          diagrams.set(id, text);
          return `<div class="blit-mermaid" id="${id}"></div>`;
        }
        const cls = lang ? ` class="language-${escapeAttr(lang)}"` : "";
        return `<pre><code${cls}>${escapeHtml(text)}</code></pre>`;
      },
    },
  });
  return marked;
}

/** Mermaid is a large dependency and most commit messages have no diagram, so
 *  it loads only when one appears. */
let mermaidModule: Promise<typeof import("mermaid")> | null = null;
function loadMermaid(): Promise<typeof import("mermaid")> {
  mermaidModule ??= import("mermaid");
  return mermaidModule;
}

/** Parse an `rgb()`/`rgba()` string into components; null if unrecognised. */
function parseRgb(value: string): [number, number, number] | null {
  const m = /rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)/.exec(value);
  return m ? [Number(m[1]), Number(m[2]), Number(m[3])] : null;
}

/**
 * Blend `color` toward `toward` by `amount`, always returning an **opaque**
 * `rgb()`.
 *
 * Mermaid runs its own lighten/darken over whatever it is given, and that
 * math misreads `rgba()` — several of the theme's surfaces are translucent,
 * so handing them over directly is what made diagrams come out washed out
 * and flat. Everything below is composited here instead.
 */
function blend(color: string, toward: string, amount: number): string {
  const a = parseRgb(color);
  const b = parseRgb(toward);
  if (!a || !b) return color;
  const c = a.map((v, i) => Math.round(v + (b[i] - v) * amount));
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

/**
 * A mermaid `themeVariables` set derived from the active terminal palette.
 *
 * Node fills are the hue mixed most of the way to the background, with the
 * hue itself as the border — so a diagram is coloured but its labels keep
 * the foreground's contrast rather than becoming light-on-light. The four
 * hues blit already derives from ANSI (accent/success/warning/error) are
 * cycled through mermaid's primary/secondary/tertiary slots and through the
 * `pie*` and `git*` series, so pie charts and git graphs get distinct
 * colours from the same palette instead of mermaid's defaults.
 */
function mermaidVars(theme: Theme): Record<string, string> {
  const { bg, fg, accent, success, warning, error } = theme;
  const fill = (hue: string) => blend(hue, bg, 0.78);
  const soft = (hue: string) => blend(hue, bg, 0.88);
  const hues = [accent, success, warning, error];
  const series: Record<string, string> = {};
  for (let i = 0; i < 8; i++) {
    const hue = hues[i % hues.length];
    // Second time round the cycle, lighten so eight slices stay distinct.
    const c = i < hues.length ? hue : blend(hue, fg, 0.35);
    series[`pie${i + 1}`] = c;
    series[`git${i}`] = c;
  }
  return {
    darkMode: String(!!parseRgb(bg) && luminance(bg) < 0.5),
    background: bg,
    fontFamily: "inherit",

    primaryColor: fill(accent),
    primaryBorderColor: accent,
    primaryTextColor: fg,
    secondaryColor: fill(success),
    secondaryBorderColor: success,
    secondaryTextColor: fg,
    tertiaryColor: fill(warning),
    tertiaryBorderColor: warning,
    tertiaryTextColor: fg,

    lineColor: blend(accent, fg, 0.25),
    textColor: fg,
    mainBkg: fill(accent),
    nodeBorder: accent,
    nodeTextColor: fg,

    // Flowchart subgraphs.
    clusterBkg: soft(accent),
    clusterBorder: blend(accent, bg, 0.55),
    titleColor: fg,
    edgeLabelBackground: bg,

    // Sequence diagrams.
    actorBkg: fill(accent),
    actorBorder: accent,
    actorTextColor: fg,
    actorLineColor: blend(fg, bg, 0.5),
    signalColor: blend(accent, fg, 0.25),
    signalTextColor: fg,
    labelBoxBkgColor: fill(success),
    labelBoxBorderColor: success,
    labelTextColor: fg,
    loopTextColor: fg,
    activationBkgColor: fill(warning),
    activationBorderColor: warning,
    noteBkgColor: fill(warning),
    noteBorderColor: warning,
    noteTextColor: fg,
    sequenceNumberColor: bg,

    // Errors, and anything mermaid flags.
    errorBkgColor: fill(error),
    errorTextColor: fg,

    ...series,
  };
}

/** Rough relative luminance, for mermaid's light/dark switch. */
function luminance(color: string): number {
  const c = parseRgb(color);
  if (!c) return 0;
  const [r, g, b] = c.map((v) => v / 255);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function CommitMarkdown(props: {
  children: string;
  theme: Theme;
  variant: "subject" | "body";
}): JSX.Element {
  const [html, setHtml] = createSignal("");
  let host: HTMLDivElement | undefined;

  createEffect(() => {
    const diagrams: Diagrams = new Map();
    const marked = buildMarked(props.theme, diagrams);
    const rendered = marked.parse(props.children ?? "", { async: false });
    setHtml(typeof rendered === "string" ? rendered : "");

    if (diagrams.size === 0) return;
    let cancelled = false;
    onCleanup(() => {
      cancelled = true;
    });
    void (async () => {
      const { default: mermaid } = await loadMermaid();
      // Mermaid needs its theme up front; it does not follow CSS variables.
      mermaid.initialize({
        startOnLoad: false,
        theme: "base",
        securityLevel: "strict",
        themeVariables: mermaidVars(props.theme),
      });
      for (const [id, source] of diagrams) {
        if (cancelled) return;
        const target = host?.querySelector<HTMLElement>(`#${id}`);
        if (!target) continue;
        try {
          const { svg } = await mermaid.render(`${id}-svg`, source);
          if (!cancelled) target.innerHTML = svg;
        } catch (err) {
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
