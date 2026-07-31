/**
 * F3 — CodeMirror 6 theme generated from the active terminal ANSI palette
 * (docs/ide-plan.md), so the editor tracks blit's palette-derived chrome and
 * regenerates on palette change. Syntax colors come from the ANSI set; UI
 * colors from the derived {@link Theme}.
 */

import { EditorView } from "@codemirror/view";
import { HighlightStyle, syntaxHighlighting } from "@codemirror/language";
import { EditorState } from "@codemirror/state";
import type { Extension } from "@codemirror/state";
import { tags as t } from "@lezer/highlight";
// Aliased: `t` in this module is already @lezer/highlight's tag set.
import { t as tr } from "../i18n";
import { measureCell, type TerminalPalette } from "@blit-sh/core";
import type { Theme } from "../theme";
import { uiScale } from "../theme";

function rgb(c: [number, number, number]): string {
  return `rgb(${c[0]}, ${c[1]}, ${c[2]})`;
}

export function cmTheme(
  theme: Theme,
  palette: TerminalPalette,
  fontFamily: string,
  fontSize: number,
): Extension {
  // The find panel is laid out with the same scale tokens the
  // project-search pane uses (ide/SearchPanel), so the two search fields
  // land on the same left edge at the same size when stacked. Deriving
  // both from uiScale is what keeps them aligned as the font size changes,
  // rather than two sets of hand-picked multiples of `fontSize`.
  const scale = uiScale(fontSize);
  const ansi = palette.ansi;
  const at = (i: number, fallback: string) =>
    ansi[i] ? rgb(ansi[i]) : fallback;
  // 1 red · 2 green · 3 yellow · 4 blue · 5 magenta · 6 cyan (+8 = bright).
  const green = at(10, theme.success);
  const yellow = at(11, theme.warning);
  const blue = at(12, theme.accent);
  const magenta = at(13, theme.accent);
  const cyan = at(14, theme.accent);
  const comment = theme.dimFg;

  // One terminal cell per line, measured the way the terminal measures it:
  // ascent + descent, snapped to device pixels (js/core/src/measure.ts).
  // Every code surface derives its line box from that one function — the
  // terminal's canvas grid, a diff or commit row, and this editor — so a
  // file, a patch and a shell stacked in three panes line up, and no glyph
  // is taller than the line that holds it.
  //
  // It stays pinned here rather than inherited from the global reset
  // (js/ui/index.html): CodeMirror's virtual scrolling *measures* the
  // rendered line height to size its viewport, so the editor states its own
  // text metrics next to the font family and size it already sets.
  const lineHeight = `${measureCell(fontFamily, fontSize).h}px`;

  const view = EditorView.theme(
    {
      "&": {
        color: theme.fg,
        backgroundColor: theme.bg,
        height: "100%",
        fontSize: `${fontSize}px`,
      },
      ".cm-content": {
        fontFamily,
        caretColor: theme.accent,
      },
      ".cm-cursor, .cm-dropCursor": { borderLeftColor: theme.accent },
      "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, .cm-content ::selection":
        { backgroundColor: theme.selectedBg },
      ".cm-gutters": {
        backgroundColor: theme.bg,
        color: theme.dimFg,
        border: "none",
      },
      ".cm-activeLine": { backgroundColor: theme.hoverBg },
      ".cm-activeLineGutter": { backgroundColor: theme.hoverBg },
      ".cm-lineNumbers .cm-gutterElement": { color: theme.dimFg },
      ".cm-scroller": { fontFamily, lineHeight },
      ".cm-tooltip": {
        backgroundColor: theme.solidPanelBg,
        color: theme.fg,
        border: `1px solid ${theme.border}`,
      },
      // Completion popup: tooltips render outside .cm-scroller, so the
      // editor font must be restated — on the ul itself, where the
      // autocomplete base theme sets its own `font-family: monospace`
      // (a direct declaration on the ul beats inheritance from the
      // tooltip div).
      ".cm-tooltip.cm-tooltip-autocomplete > ul": {
        fontFamily,
        fontSize: `${Math.round(fontSize * 0.92)}px`,
      },
      ".cm-tooltip-autocomplete ul li[aria-selected]": {
        backgroundColor: theme.selectedBg,
        color: theme.fg,
      },
      ".cm-completionMatchedText": {
        textDecoration: "none",
        color: blue,
      },
      ".cm-completionDetail": {
        color: theme.dimFg,
        fontStyle: "normal",
        marginLeft: "1.5em",
      },
      ".cm-completionIcon": { color: theme.dimFg },
      // Signature help (BlitEditor's hand-rolled tooltip).
      ".cm-tooltip .blit-signature": {
        fontFamily,
        fontSize: `${Math.round(fontSize * 0.92)}px`,
        padding: "3px 6px",
        maxWidth: "72ch",
      },
      ".blit-signature b": { color: blue, fontWeight: "700" },
      ".blit-signature-count": { color: theme.dimFg },
      ".blit-signature-doc": { color: theme.dimFg, marginTop: "2px" },
      // Hover (BlitEditor's hand-rolled tooltip): a signature-shaped box
      // whose fenced code blocks keep the editor font and the prose
      // around them dims back.
      ".cm-tooltip .blit-hover": {
        fontFamily,
        fontSize: `${Math.round(fontSize * 0.92)}px`,
        padding: "3px 6px",
        maxWidth: "72ch",
        maxHeight: "20em",
        overflowY: "auto",
      },
      ".blit-hover pre": {
        margin: "0 0 2px 0",
        whiteSpace: "pre-wrap",
        color: theme.fg,
      },
      ".blit-hover p": { margin: "2px 0", color: theme.dimFg },
      ".blit-hover code": { color: cyan },
      ".blit-hover hr": {
        border: "none",
        borderTop: `1px solid ${theme.subtleBorder}`,
        margin: "3px 0",
      },
      // Occurrences of the current selection, and search-panel matches.
      ".cm-selectionMatch": { backgroundColor: theme.hoverBg },
      ".cm-searchMatch": {
        backgroundColor: "transparent",
        outline: `1px solid ${yellow}`,
      },
      ".cm-searchMatch.cm-searchMatch-selected": {
        backgroundColor: theme.selectedBg,
      },
      ".cm-trailingSpace": { backgroundColor: theme.hoverBg },
      // The search panel docks inside the editor, so it has to be themed
      // out of the CM default (a light-mode bar) like every other chrome.
      // The in-file search panel, styled to match the project-search pane
      // (ide/SearchPanel). CodeMirror ships it as bare browser chrome —
      // native checkboxes and buttons on a light bar — which reads as a
      // foreign dialog dropped into the editor. Same tokens, same compact
      // rhythm, so the two searches look like the same product.
      ".cm-panels": {
        backgroundColor: theme.panelBg,
        color: theme.fg,
        fontFamily,
        fontSize: `${Math.round(fontSize * 0.85)}px`,
      },
      ".cm-panels.cm-panels-top": { borderBottom: `1px solid ${theme.border}` },
      ".cm-panels.cm-panels-bottom": { borderTop: `1px solid ${theme.border}` },
      // Our own find/replace panel (ide/cmSearchPanel). CodeMirror's was
      // replaced rather than restyled — see that module for why CSS alone
      // could not get there.
      ".blit-cm-search": {
        display: "flex",
        flexDirection: "column",
        gap: `${scale.tightGap}px`,
        // Same padding as the project-search pane's header wrapper.
        padding: `${scale.tightGap}px`,
        position: "relative",
      },
      ".blit-cm-search-row": {
        display: "flex",
        alignItems: "center",
        gap: `${scale.tightGap}px`,
      },
      ".blit-cm-search-field": {
        fontFamily,
        // border-box, as the project field is: with content-box the padding
        // and border sit outside the flex basis, so the two fields resolve
        // their widths differently.
        boxSizing: "border-box",
        // `md`, matching the project-search query field — the panel's own
        // 85% size made the two look like different controls.
        fontSize: `${scale.md}px`,
        color: theme.fg,
        background: "transparent",
        border: `1px solid ${theme.subtleBorder}`,
        borderRadius: "2px",
        outline: "none",
        padding: "1px 4px",
        // Grows to fill, as the project-search field does; the chips keep
        // their natural width on the right.
        flex: "1 1 auto",
        minWidth: 0,
      },
      ".blit-cm-search-field:focus": { borderColor: theme.accent },
      ".blit-cm-search-count": {
        fontSize: `${scale.sm}px`,
        color: theme.dimFg,
        fontVariantNumeric: "tabular-nums",
        whiteSpace: "nowrap",
        flexShrink: 0,
        minWidth: "4ch",
        textAlign: "right",
      },
      '.blit-cm-search-count[data-state="error"]': { color: theme.errorText },
      // Chips and actions are one visual family: dim, flat, no border.
      // `aria-pressed` is the style hook as well as the accessible state,
      // so an "on" chip cannot look off.
      ".blit-cm-search-chip, .blit-cm-search-action": {
        fontFamily,
        // `sm`, as the project-search toggles are.
        fontSize: `${scale.sm}px`,
        color: theme.dimFg,
        background: "transparent",
        backgroundImage: "none",
        border: "none",
        borderRadius: "2px",
        cursor: "pointer",
        // Identical to the project-search toggles.
        padding: "2px 6px",
        whiteSpace: "nowrap",
        flexShrink: 0,
      },
      ".blit-cm-search-chip:hover, .blit-cm-search-action:hover": {
        color: theme.fg,
        backgroundColor: theme.hoverBg,
      },
      '.blit-cm-search-chip[aria-pressed="true"]': {
        color: theme.fg,
        backgroundColor: theme.selectedBg,
      },
      ".blit-cm-search-close": {
        color: theme.dimFg,
        marginLeft: "2px",
      },
    },
    { dark: palette.dark },
  );

  const highlight = HighlightStyle.define([
    { tag: t.keyword, color: magenta },
    { tag: [t.controlKeyword, t.moduleKeyword], color: magenta },
    { tag: [t.typeName, t.className, t.namespace], color: yellow },
    {
      tag: [t.function(t.variableName), t.function(t.propertyName)],
      color: blue,
    },
    { tag: [t.string, t.special(t.string)], color: green },
    { tag: [t.number, t.bool, t.atom], color: cyan },
    {
      tag: [t.comment, t.lineComment, t.blockComment, t.docComment],
      color: comment,
      fontStyle: "italic",
    },
    // Not red: red is for things that are wrong (t.invalid below, and
    // the diagnostic squiggles). Rust macros are t.macroName and its
    // attributes are t.meta, so a red mapping lit up every println!
    // and #[derive] in the file as if it were broken.
    { tag: t.macroName, color: magenta },
    { tag: t.meta, color: cyan },
    {
      tag: [t.operator, t.punctuation, t.separator, t.bracket],
      color: theme.dimFg,
    },
    { tag: [t.propertyName, t.attributeName], color: cyan },
    { tag: t.invalid, color: theme.errorText },
  ]);

  return [view, syntaxHighlighting(highlight)];
}

/**
 * CodeMirror's own UI strings — the search panel's "Find"/"next"/"match
 * case", the goto-line prompt, the lint panel — routed through the app's
 * i18n table via `EditorState.phrases`. Styling the panel to match the
 * product only fixed half the mismatch; it still spoke English in every
 * locale.
 *
 * `t()` falls back to English for any key a locale lacks, so an
 * untranslated phrase reads as CodeMirror's own original wording.
 */
export function cmPhrases(): Extension {
  const keys = [
    "current match",
    "on line",
    "Go to line",
    "go",
    "Diagnostics",
    "No diagnostics",
    "replaced $ matches",
    "replaced match on line $",
  ];
  const phrases: Record<string, string> = {};
  for (const k of keys) phrases[k] = tr(`cm.${k}`);
  return EditorState.phrases.of(phrases);
}
