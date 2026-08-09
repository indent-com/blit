/**
 * BlitPreview — a file rendered rather than shown as source.
 *
 * Images, SVG, markdown and HTML get a Preview tab in the status bar
 * (ide/FileViewSwitcher), which opens this tile. Everything is read-only:
 * there is no write path here at all, which is why the fs sync it opens is
 * far simpler than BlitEditor's — no CAS base, no dir fallback, no
 * conflict handling.
 *
 * **Embedded assets are fetched through the same connection.** A browser
 * cannot resolve `![](diagram.png)` or `<img src="logo.svg">` against the
 * server's filesystem, so every relative reference is resolved, fetched
 * over the fs family, and swapped for a blob URL before the content is
 * shown. That is what makes a real README render rather than a page of
 * broken-image icons.
 *
 * HTML renders in a sandboxed iframe with **no** `allow-scripts` and no
 * `allow-same-origin`: previewing a file from a repository must not let it
 * run code in the app's origin or read its storage. A preview that cannot
 * execute is worth more than one that can.
 */

import {
  createEffect,
  createMemo,
  createSignal,
  onCleanup,
  Show,
} from "solid-js";
import type { BlitWorkspace, ConnectionId, FsSyncHandle } from "@blit-sh/core";
import { Marked } from "marked";
import { createBlitWorkspaceState } from "@blit-sh/solid";
import type { Theme, UIScale } from "../theme";
import { scrollbarStyle } from "../theme";
import { isConnReady, connGeneration } from "./reactive";
import {
  clearActiveEditor,
  setActiveEditorFocused,
  type PreviewController,
} from "./activeEditor";
import {
  previewKindFor,
  previewMime,
  resolveRelative,
  type PreviewKind,
} from "./previewKind";
import "./commitMarkdown.css";

/** Cap on an embedded asset. A preview should not pull a video in because
 *  a page linked one. */
const ASSET_MAX = 8 * 1024 * 1024;

export function BlitPreview(props: {
  workspace: BlitWorkspace;
  connectionId: ConnectionId;
  path: string;
  theme: Theme;
  scale: UIScale;
  fontFamily: string;
  fontSize: number;
  onOpenTile: (assignment: string) => void;
  /** Read-only thumbnail (the background dock): no status-bar claim. */
  preview?: boolean;
  /** Whether this tile's workspace pane is focused. */
  focused?: boolean;
}) {
  const kind = createMemo<PreviewKind | null>(() => previewKindFor(props.path));
  const [bytes, setBytes] = createSignal<Uint8Array | null>(null);
  const [error, setError] = createSignal<string | null>(null);
  const [html, setHtml] = createSignal("");
  const [imageUrl, setImageUrl] = createSignal<string | null>(null);

  // Every blob URL this tile minted, revoked together on teardown or on the
  // next render — an un-revoked blob pins its bytes for the page's life.
  let urls: string[] = [];
  const mint = (data: Uint8Array, mime: string): string => {
    const url = URL.createObjectURL(
      new Blob([data as unknown as BlobPart], { type: mime }),
    );
    urls.push(url);
    return url;
  };
  const revokeAll = () => {
    for (const u of urls) URL.revokeObjectURL(u);
    urls = [];
  };
  onCleanup(revokeAll);

  const wsState = createBlitWorkspaceState(props.workspace);
  const connConnected = createMemo(() =>
    isConnReady(wsState(), props.connectionId, "supportsFsSync"),
  );
  const connGen = createMemo(() =>
    connGeneration(wsState(), props.connectionId),
  );

  const controller: PreviewController = {
    kind: "preview",
    connectionId: props.connectionId,
    path: props.path,
    onOpenTile: props.onOpenTile,
  };
  createEffect(() => {
    setActiveEditorFocused(
      controller,
      !props.preview && props.focused !== false,
    );
  });
  onCleanup(() => clearActiveEditor(controller));

  /** Fetch one path's bytes through a short-lived single-file sync.
   *  Embedded assets are one-shot reads, so they do not hold a sync open. */
  async function readFile(path: string): Promise<Uint8Array | null> {
    let handle: FsSyncHandle | null = null;
    try {
      handle = await props.workspace.syncFs(props.connectionId, path, {
        single: true,
        content: true,
        inlineMax: ASSET_MAX,
      });
      const node = handle.live.get("");
      if (node?.content) return node.content;
      return await handle.fetch("");
    } catch {
      return null;
    } finally {
      handle?.stop();
    }
  }

  // The file itself.
  createEffect(() => {
    const path = props.path;
    connGen();
    if (!connConnected()) return;
    let cancelled = false;
    setError(null);
    void (async () => {
      const data = await readFile(path);
      if (cancelled) return;
      if (!data) {
        setError("cannot read this file");
        return;
      }
      setBytes(data);
    })();
    onCleanup(() => {
      cancelled = true;
    });
  });

  // Render, once bytes are in hand. Rendering is per-kind and each branch
  // resolves its own embedded assets.
  createEffect(() => {
    const data = bytes();
    const k = kind();
    if (!data || !k) return;
    let cancelled = false;
    onCleanup(() => {
      cancelled = true;
    });
    void (async () => {
      revokeAll();
      if (k === "image" || k === "svg") {
        setImageUrl(mint(data, previewMime(props.path)));
        return;
      }
      const text = new TextDecoder("utf-8", { fatal: false }).decode(data);
      if (k === "markdown") {
        const rendered = await renderMarkdown(text);
        if (!cancelled) setHtml(rendered);
      } else {
        const rendered = await renderHtml(text);
        if (!cancelled) setHtml(rendered);
      }
    })();
  });

  /** Fetch every relative reference in `paths`, as blob URLs. */
  async function assetUrls(refs: string[]): Promise<Map<string, string>> {
    const out = new Map<string, string>();
    const unique = [...new Set(refs)];
    await Promise.all(
      unique.map(async (ref) => {
        const abs = resolveRelative(props.path, ref);
        if (!abs) return;
        const data = await readFile(abs);
        if (data) out.set(ref, mint(data, previewMime(abs)));
      }),
    );
    return out;
  }

  async function renderMarkdown(text: string): Promise<string> {
    // Two passes: walk the tokens to collect image references, fetch them,
    // then render with a renderer that substitutes blob URLs. marked's
    // renderer is synchronous, so the fetching cannot happen inside it.
    const lexer = new Marked();
    const refs: string[] = [];
    const walk = (tokens: unknown[]): void => {
      for (const tok of tokens as {
        type?: string;
        href?: string;
        tokens?: unknown[];
        items?: unknown[];
      }[]) {
        if (tok.type === "image" && tok.href) refs.push(tok.href);
        if (tok.tokens) walk(tok.tokens);
        if (tok.items) walk(tok.items);
      }
    };
    walk(lexer.lexer(text));
    const assets = await assetUrls(refs);

    const marked = new Marked({
      // Raw HTML in the document is dropped, as it is for commit text: a
      // preview must not become an injection point into the app's DOM.
      async: false,
      gfm: true,
      breaks: false,
    });
    marked.use({
      renderer: {
        html: () => "",
        image({ href, title, text: alt }) {
          const url = assets.get(href ?? "");
          if (!url) {
            // Unresolvable (remote, or missing on disk): show the alt text
            // rather than a broken-image glyph.
            return `<em>${escapeHtml(alt ?? href ?? "")}</em>`;
          }
          const t = title ? ` title="${escapeAttr(title)}"` : "";
          return `<img src="${escapeAttr(url)}" alt="${escapeAttr(alt ?? "")}"${t} loading="lazy" decoding="async">`;
        },
        link({ href, text: label }) {
          // Links open nothing: a preview is a viewer, and a relative link
          // would resolve against the app's own origin.
          return `<span class="blit-md-link" title="${escapeAttr(href ?? "")}">${escapeHtml(label ?? "")}</span>`;
        },
      },
    });
    const out = marked.parse(text, { async: false });
    return typeof out === "string" ? out : "";
  }

  async function renderHtml(text: string): Promise<string> {
    // Parse rather than regex so attribute rewriting cannot be fooled by
    // quoting. The document never touches this page's DOM — it is
    // serialized back out and handed to a sandboxed iframe.
    const doc = new DOMParser().parseFromString(text, "text/html");
    const attrs: [string, string][] = [
      ["img", "src"],
      ["source", "src"],
      ["video", "poster"],
      ["link", "href"],
    ];
    const refs: string[] = [];
    for (const [tag, attr] of attrs) {
      for (const el of doc.querySelectorAll(tag)) {
        const v = el.getAttribute(attr);
        if (v && resolveRelative(props.path, v)) refs.push(v);
      }
    }
    const assets = await assetUrls(refs);
    for (const [tag, attr] of attrs) {
      for (const el of doc.querySelectorAll(tag)) {
        const v = el.getAttribute(attr);
        if (!v) continue;
        const url = assets.get(v);
        if (url) el.setAttribute(attr, url);
        else if (resolveRelative(props.path, v)) el.removeAttribute(attr);
      }
    }
    // Scripts go, belt and braces: the iframe already refuses to run them,
    // but a stripped document cannot surprise a future change of sandbox.
    for (const el of doc.querySelectorAll("script")) el.remove();
    return doc.documentElement.outerHTML;
  }

  return (
    <div
      style={{
        width: "100%",
        height: "100%",
        overflow: "auto",
        background: props.theme.bg,
        color: props.theme.fg,
        ...scrollbarStyle(props.theme),
      }}
    >
      <Show when={error()}>
        <div
          style={{
            padding: `${props.scale.panelPadding}px`,
            "font-family": props.fontFamily,
            "font-size": `${props.scale.md}px`,
            color: props.theme.errorText,
          }}
        >
          {error()}
        </div>
      </Show>
      <Show when={!kind()}>
        <div
          style={{
            padding: `${props.scale.panelPadding}px`,
            "font-family": props.fontFamily,
            "font-size": `${props.scale.md}px`,
            color: props.theme.dimFg,
          }}
        >
          No preview for this file type.
        </div>
      </Show>
      <Show when={imageUrl()}>
        {/* Checkerboard so a transparent PNG reads as transparent rather
            than as whatever the theme background happens to be. */}
        <div
          style={{
            display: "flex",
            "align-items": "center",
            "justify-content": "center",
            "min-height": "100%",
            padding: `${props.scale.panelPadding}px`,
            "background-image":
              "linear-gradient(45deg, rgba(128,128,128,0.15) 25%, transparent 25%, transparent 75%, rgba(128,128,128,0.15) 75%), linear-gradient(45deg, rgba(128,128,128,0.15) 25%, transparent 25%, transparent 75%, rgba(128,128,128,0.15) 75%)",
            "background-size": "16px 16px",
            "background-position": "0 0, 8px 8px",
          }}
        >
          <img
            src={imageUrl()!}
            alt={props.path}
            style={{
              "max-width": "100%",
              "max-height": "100%",
              "object-fit": "contain",
              "image-rendering": "pixelated",
            }}
          />
        </div>
      </Show>
      <Show when={kind() === "markdown" && html()}>
        <div
          // The body variant carries the prose styling; the base class
          // alone only sets margins.
          class="blit-commit-markdown blit-commit-markdown--body"
          style={{
            padding: `${props.scale.panelPadding}px`,
            "font-size": `${props.scale.md}px`,
            "line-height": 1.5,
          }}
          innerHTML={html()}
        />
      </Show>
      <Show when={kind() === "html" && html()}>
        <iframe
          title={props.path}
          srcdoc={html()}
          // No allow-scripts, no allow-same-origin: a repository file gets
          // to be looked at, not to run.
          sandbox=""
          referrerpolicy="no-referrer"
          style={{
            width: "100%",
            height: "100%",
            border: "none",
            background: "#fff",
          }}
        />
      </Show>
    </div>
  );
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function escapeAttr(value: string): string {
  return escapeHtml(value).replace(/"/g, "&quot;");
}
