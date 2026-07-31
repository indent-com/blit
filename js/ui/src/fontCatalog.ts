/**
 * The faces a host can actually render, when it is not a blit server.
 *
 * The app's font picker is a search box over the families the server reports
 * installed, plus free text for anything else — both rest on `font/<family>`
 * serving the face on demand. An embedder has no such route: it can only
 * offer what it bundled into the page. So the picker's choices come from the
 * host instead, and typing a family name is not offered, because a name the
 * page never shipped resolves to nothing and looks like a bug in the picker.
 *
 * Label and value are separate because what gets applied is a stack, not a
 * name: a face plus the fallbacks to draw with while it loads, or if it never
 * does. "JetBrains Mono" is the menu entry; `"JetBrains Mono", ui-monospace,
 * monospace` is the setting. Hosts whose bundler renames the faces it subsets
 * need the split more urgently still — though a host that persists such a name
 * has a saved preference that expires on its next deploy, so the better fix is
 * upstream, in how the face is declared.
 *
 * Page-level like the shell capabilities and the default font, for the same
 * reason: it describes the document, and is set once before mount.
 */

export interface FontChoice {
  /** What the picker shows. */
  label: string;
  /** The `font-family` stack to apply. */
  stack: string;
}

let catalog: readonly FontChoice[] = [];

export function setFontCatalog(next: readonly FontChoice[]): void {
  catalog = [...next];
}

export function fontCatalog(): readonly FontChoice[] {
  return catalog;
}
