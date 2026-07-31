/**
 * Nix language support, vendored from @replit/codemirror-lang-nix 6.0.1
 * (MIT) — the only published Nix grammar for CodeMirror 6, unmaintained
 * since 2023 and wrong on two constructs this repo's own .nix files use
 * constantly:
 *
 *  - a trailing comma in a formal argument set (`{ a, b, }:`), which is
 *    what nixfmt emits for anything multi-line;
 *  - the `''` escapes inside indented strings (`''${`, `'''`, `''\n`).
 *
 * The second was the worse of the two: `''${VAR}` ended the string early,
 * so the rest of the file parsed as code — which is why `#` and `/*` in
 * embedded shell snippets lit up as comments.
 *
 * Fixes live in ./syntax.grammar (formals) and ./tokens.ts
 * (scanIndString); both are commented at the point of change. Everything
 * else is upstream. syntax.grammar compiles at build time via the
 * @lezer/generator rollup plugin in vite.config.ts.
 */
import { parser as nixParser } from "./syntax.grammar";
import {
  LRLanguage,
  LanguageSupport,
  indentNodeProp,
  foldNodeProp,
  foldInside,
  delimitedIndent,
  continuedIndent,
} from "@codemirror/language";
import { styleTags, tags as t } from "@lezer/highlight";
import {
  completeFromList,
  ifNotIn,
  snippetCompletion as snip,
  type Completion,
} from "@codemirror/autocomplete";

export const parser = nixParser;

export const nixLanguage = LRLanguage.define({
  name: "Nix",
  parser: parser.configure({
    props: [
      indentNodeProp.add({
        Parenthesized: delimitedIndent({ closing: ")" }),
        AttrSet: delimitedIndent({ closing: "}" }),
        List: delimitedIndent({ closing: "]" }),
        Let: continuedIndent({ except: /^\s*in\b/ }),
      }),
      foldNodeProp.add({
        AttrSet: foldInside,
        List: foldInside,
        Let(node) {
          let first = node.getChild("let"),
            last = node.getChild("in");
          if (!first || !last) return null;
          return { from: first.to, to: last.from };
        },
      }),
      styleTags({
        Identifier: t.propertyName,
        Boolean: t.bool,
        String: t.string,
        IndentedString: t.string,
        LineComment: t.lineComment,
        BlockComment: t.blockComment,
        Float: t.float,
        Integer: t.integer,
        Null: t.null,
        URI: t.url,
        SPath: t.literal,
        Path: t.literal,
        "( )": t.paren,
        "{ }": t.brace,
        "[ ]": t.squareBracket,
        "if then else": t.controlKeyword,
        "import with let in rec builtins inherit assert or": t.keyword,
      }),
    ],
  }),
  languageData: {
    commentTokens: { line: "#", block: { open: "/*", close: "*/" } },
    closeBrackets: { brackets: ["(", "[", "{", "''", '"'] },
    indentOnInput: /^\s*(in|\}|\)|\])$/,
  },
});

const snippets: readonly Completion[] = [
  snip("let ${binds} in ${expression}", {
    label: "let",
    detail: "Let ... in statement",
    type: "keyword",
  }),
  snip("with ${expression}; ${expression}", {
    label: "with",
    detail: "With statement",
    type: "keyword",
  }),
];

export function nix() {
  return new LanguageSupport(
    nixLanguage,
    nixLanguage.data.of({
      autocomplete: ifNotIn(
        ["LineComment", "BlockComment", "String", "IndentedString"],
        completeFromList(snippets),
      ),
    }),
  );
}
