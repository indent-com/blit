/**
 * Standalone syntax highlighting for the diff viewer.
 *
 * BlitDiff renders its own rows (not a CodeMirror editor), so it can't lean on
 * CM's `syntaxHighlighting`. Instead we drive the language's Lezer parser
 * directly and map highlight tags to palette colors — the same tag→color scheme
 * as {@link cmTheme}, so the diff and the editor look consistent.
 *
 * Highlighting is per-line: each hunk row is parsed on its own. Cross-line
 * constructs (block comments, multi-line strings) aren't tracked, which is an
 * acceptable approximation for a diff.
 */

import {
  highlightTree,
  tags as t,
  type Tag,
  type Highlighter,
} from "@lezer/highlight";
import type { LanguageSupport } from "@codemirror/language";
import type { TerminalPalette } from "@blit-sh/core";
import type { Theme } from "../theme";

function rgb(c: [number, number, number]): string {
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

// One highlighter per (theme, palette) pair, so every diff/commit tile
// shares one identity and the line-color cache below stays coherent
// across tiles.
const highlighterCache = new WeakMap<
  Theme,
  WeakMap<TerminalPalette, Highlighter>
>();

/** Build a tag→color highlighter mirroring cmTheme's palette-derived scheme.
 *  Cached by (theme, palette) identity. */
export function buildDiffHighlighter(
  theme: Theme,
  palette: TerminalPalette,
): Highlighter {
  let byPalette = highlighterCache.get(theme);
  if (!byPalette) {
    byPalette = new WeakMap();
    highlighterCache.set(theme, byPalette);
  }
  const cached = byPalette.get(palette);
  if (cached) return cached;
  const built = makeHighlighter(theme, palette);
  byPalette.set(palette, built);
  return built;
}

function makeHighlighter(theme: Theme, palette: TerminalPalette): Highlighter {
  const ansi = palette.ansi;
  const at = (i: number, fallback: string) =>
    ansi[i] ? rgb(ansi[i]) : fallback;
  const green = at(10, theme.success);
  const yellow = at(11, theme.warning);
  const blue = at(12, theme.accent);
  const magenta = at(13, theme.accent);
  const cyan = at(14, theme.accent);
  const comment = theme.dimFg;

  const map = new Map<Tag, string>();
  const put = (tags: readonly Tag[], color: string) =>
    tags.forEach((tag) => map.set(tag, color));
  put([t.keyword, t.controlKeyword, t.moduleKeyword], magenta);
  put([t.typeName, t.className, t.namespace], yellow);
  put([t.function(t.variableName), t.function(t.propertyName)], blue);
  put([t.string, t.special(t.string)], green);
  put([t.number, t.bool, t.atom], cyan);
  put([t.comment, t.lineComment, t.blockComment, t.docComment], comment);
  // Red is reserved for t.invalid, matching cm-theme: a macro is not
  // an error.
  put([t.macroName], magenta);
  put([t.meta], cyan);
  put([t.operator, t.punctuation, t.separator, t.bracket], theme.dimFg);
  put([t.propertyName, t.attributeName], cyan);
  put([t.invalid], theme.errorText);

  return {
    style(tags: readonly Tag[]): string | null {
      for (const tag of tags) {
        // `tag.set` walks the tag and the tags it derives from, matching the
        // inheritance HighlightStyle uses.
        for (const anc of tag.set) {
          const color = map.get(anc);
          if (color) return color;
        }
      }
      return null;
    },
  };
}

/**
 * Per-character syntax colors for one line of text (index i → color or null).
 * Returns all-null when there's no language (plain text) or the line is empty.
 *
 * Cached by (highlighter, language, line content): diffs repeat lines across
 * refetches (and the split view shows a context line twice), so each distinct
 * line parses once. A theme/palette change swaps the highlighter identity and
 * drops the cache wholesale; per-language maps are cleared past a cap rather
 * than tracked LRU — a diff refill is cheap next to unbounded growth.
 */
const LINE_CACHE_MAX = 50_000;
let lineCacheHl: Highlighter | null = null;
const lineCache = new Map<LanguageSupport, Map<string, (string | null)[]>>();

export function lineColors(
  text: string,
  lang: LanguageSupport | null,
  hl: Highlighter,
): (string | null)[] {
  if (!lang || text.length === 0) {
    return new Array<string | null>(text.length).fill(null);
  }
  if (hl !== lineCacheHl) {
    lineCache.clear();
    lineCacheHl = hl;
  }
  let byText = lineCache.get(lang);
  if (!byText) {
    byText = new Map();
    lineCache.set(lang, byText);
  }
  const cached = byText.get(text);
  if (cached) return cached;
  const colors: (string | null)[] = new Array(text.length).fill(null);
  const tree = lang.language.parser.parse(text);
  highlightTree(tree, hl, (from, to, color) => {
    for (let i = from; i < to && i < colors.length; i++) colors[i] = color;
  });
  if (byText.size >= LINE_CACHE_MAX) byText.clear();
  byText.set(text, colors);
  return colors;
}
