/**
 * LSP SymbolKind → a short tag. The names are the LSP ones, abbreviated to
 * stay in one narrow column: the editor's outline lists them in a gutter,
 * and the switcher's `#` mode puts them beside each hit.
 */
const SYMBOL_KINDS: Record<number, string> = {
  1: "file",
  2: "mod",
  3: "ns",
  4: "pkg",
  5: "class",
  6: "method",
  7: "prop",
  8: "field",
  9: "ctor",
  10: "enum",
  11: "iface",
  12: "fn",
  13: "var",
  14: "const",
  15: "str",
  16: "num",
  17: "bool",
  18: "array",
  19: "obj",
  20: "key",
  21: "null",
  22: "member",
  23: "struct",
  24: "event",
  25: "op",
  26: "typar",
};

/** The tag for an LSP SymbolKind; "?" for values outside the spec. */
export function symbolKindTag(symKind: number): string {
  return SYMBOL_KINDS[symKind] ?? "?";
}
