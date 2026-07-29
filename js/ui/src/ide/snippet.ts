/**
 * LSP snippet syntax → CodeMirror snippet syntax.
 *
 * The two are close but not identical, and the differences all matter:
 *
 * - LSP allows bare `$1` tabstops; CodeMirror only understands `${1}`.
 * - LSP has choices, `${1|a,b,c|}`. CodeMirror has no choice UI, so the
 *   first option becomes an ordinary placeholder default — better than
 *   showing the user the raw `|a,b,c|`.
 * - LSP escapes are `\$`, `\}`, `\\`. CodeMirror instead treats `{` and
 *   `}` as needing a backslash, and `$` as literal unless followed by `{`.
 * - LSP variables (`$TM_FILENAME`, `${TM_SELECTED_TEXT:default}`) have no
 *   CodeMirror equivalent; they resolve to their default, or to nothing.
 *
 * `$0` is the final cursor stop in both, so it passes through.
 *
 * Anything not recognised is emitted literally with braces escaped, so a
 * malformed template degrades to text rather than to a template that eats
 * the following code.
 */

/** LSP variables worth resolving locally; everything else drops to "". */
const VARS = new Set([
  "TM_SELECTED_TEXT",
  "TM_CURRENT_LINE",
  "TM_CURRENT_WORD",
  "TM_LINE_INDEX",
  "TM_LINE_NUMBER",
  "TM_FILENAME",
  "TM_FILENAME_BASE",
  "TM_DIRECTORY",
  "TM_FILEPATH",
  "CLIPBOARD",
  "WORKSPACE_NAME",
  "WORKSPACE_FOLDER",
]);

/** Escape the characters CodeMirror's snippet parser treats as syntax. */
function literal(text: string): string {
  return text.replace(/[{}]/g, "\\$&");
}

export function lspSnippetToCm(template: string): string {
  let out = "";
  let i = 0;
  while (i < template.length) {
    const ch = template[i];

    // LSP escape: backslash protects $, }, { and itself. Everything else
    // keeps the backslash, since it is ordinary text (a regex, a path).
    if (ch === "\\" && i + 1 < template.length) {
      const next = template[i + 1];
      out += "$}{\\".includes(next) ? literal(next) : literal("\\" + next);
      i += 2;
      continue;
    }

    if (ch !== "$") {
      out += literal(ch);
      i++;
      continue;
    }

    // `$1` / `$0` — a bare tabstop.
    const bare = /^\$(\d+)/.exec(template.slice(i));
    if (bare) {
      out += `\${${bare[1]}}`;
      i += bare[0].length;
      continue;
    }

    // `$NAME` — a bare variable.
    const bareVar = /^\$([A-Za-z_][A-Za-z0-9_]*)/.exec(template.slice(i));
    if (bareVar) {
      // No local value for any of them, so a known variable contributes
      // nothing and an unknown name stays literal (it is likelier to be
      // prose than a variable the server expected us to expand).
      out += VARS.has(bareVar[1]) ? "" : literal(bareVar[0]);
      i += bareVar[0].length;
      continue;
    }

    if (template[i + 1] !== "{") {
      // A lone `$`. Literal in CodeMirror as long as no `{` follows, which
      // it does not.
      out += "$";
      i++;
      continue;
    }

    // `${...}` — find the matching brace, honouring nesting and escapes so
    // a default value containing braces does not truncate the placeholder.
    let depth = 0;
    let j = i + 1;
    for (; j < template.length; j++) {
      if (template[j] === "\\") {
        j++;
        continue;
      }
      if (template[j] === "{") depth++;
      else if (template[j] === "}" && --depth === 0) break;
    }
    if (j >= template.length) {
      // Unterminated: emit the rest as text rather than as a template.
      out += literal(template.slice(i));
      break;
    }
    const body = template.slice(i + 2, j);
    i = j + 1;

    // `${1|a,b,c|}` — a choice. Take the first option as the default.
    const choice = /^(\d+)\|(.*)\|$/.exec(body);
    if (choice) {
      const first = choice[2].split(",")[0] ?? "";
      out += first ? `\${${choice[1]}:${literal(first)}}` : `\${${choice[1]}}`;
      continue;
    }

    // `${1}` or `${1:default}`. The default is itself a template — nested
    // tabstops are legal — so it recurses.
    const stop = /^(\d+)(?::([\s\S]*))?$/.exec(body);
    if (stop) {
      out +=
        stop[2] === undefined
          ? `\${${stop[1]}}`
          : `\${${stop[1]}:${lspSnippetToCm(stop[2])}}`;
      continue;
    }

    // `${NAME}` / `${NAME:default}` — a variable. Resolve to its default.
    const v = /^([A-Za-z_][A-Za-z0-9_]*)(?::([\s\S]*))?$/.exec(body);
    if (v) {
      out += v[2] !== undefined ? lspSnippetToCm(v[2]) : "";
      continue;
    }

    // Regex-transform forms (`${1/…/…/}`) and anything else unrecognised:
    // drop the construct rather than paste its source into the buffer.
    const num = /^(\d+)/.exec(body);
    out += num ? `\${${num[1]}}` : "";
  }
  return out;
}

// ---------------------------------------------------------------------------
// Indentation detection
// ---------------------------------------------------------------------------

/**
 * Guess a file's indentation from its own content.
 *
 * CodeMirror defaults to two spaces for `indentUnit` and eight for
 * `tabSize`, neither of which is likely to match the file in front of you —
 * so pressing Tab in a 4-space Rust file or a tab-indented Makefile
 * inserted the wrong thing and re-indenting a block silently reformatted
 * it. There is no `.editorconfig` reader here, and the document is the more
 * reliable witness anyway: whatever the project's stated policy, the file's
 * existing lines are what the next line has to match.
 *
 * Counts the *step* between consecutive indent levels rather than the
 * absolute width of each line, which is what distinguishes 2-space code
 * from 4-space code whose blocks happen to nest twice.
 */
export function detectIndent(
  text: string,
  limit = 500,
): { unit: string; tabSize: number } {
  const lines = text.split("\n", limit);
  let tabs = 0;
  let spaces = 0;
  const steps = new Map<number, number>();
  let prev = 0;
  for (const line of lines) {
    if (!line.trim()) continue; // blank lines say nothing
    const m = /^[\t ]*/.exec(line)![0];
    if (m.includes("\t")) tabs++;
    const width = m.length - m.replace(/ /g, "").length;
    if (m.includes(" ")) spaces++;
    if (width > prev) {
      const step = width - prev;
      // A step over 8 is a continuation line aligned to a paren, not an
      // indent level.
      if (step <= 8) steps.set(step, (steps.get(step) ?? 0) + 1);
    }
    prev = width;
  }
  // Tabs win only when they clearly dominate: a space-indented file with a
  // few stray tabs should still indent with spaces.
  if (tabs > spaces) return { unit: "\t", tabSize: 4 };
  let best = 0;
  let bestCount = 0;
  for (const [step, count] of steps) {
    // Ties go to the smaller step: 4-space code produces plenty of 4s and
    // some 8s, and 8 is never the unit when 4 is equally common.
    if (count > bestCount || (count === bestCount && step < best)) {
      best = step;
      bestCount = count;
    }
  }
  const unit = best >= 2 ? best : 2;
  return { unit: " ".repeat(unit), tabSize: unit };
}
