/**
 * File → CodeMirror language support, loaded on demand from
 * `@codemirror/language-data` (≈140 languages). Nothing but the small
 * descriptor table (names, extensions, dynamic-import loaders) is eager; a
 * language's grammar is fetched the first time a matching file is shown.
 *
 * Two entry points share one cache:
 *  - `langForFile` is synchronous for reactive callers (the diff and commit
 *    highlighters): it returns null until the grammar loads, then an internal
 *    signal ticks so they re-run and pick it up.
 *  - `loadLangForFile` returns a promise, for the editor to reconfigure its
 *    language compartment once the grammar resolves.
 */
import { createSignal } from "solid-js";
import { languages } from "@codemirror/language-data";
import {
  LanguageDescription,
  type LanguageSupport,
} from "@codemirror/language";

/**
 * Languages `@codemirror/language-data` does not ship at all. Declared the
 * same way its own table is, so the grammar is still fetched lazily on
 * first use and nothing here is eager.
 */
const LOCAL_LANGUAGES: LanguageDescription[] = [
  LanguageDescription.of({
    name: "Nix",
    extensions: ["nix"],
    load: () => import("./nix").then((m) => m.nix()),
  }),
];

/** The local table first, then everything language-data knows. */
const ALL_LANGUAGES = [...LOCAL_LANGUAGES, ...languages];

/**
 * Filenames `@codemirror/language-data` does not recognize, mapped to a
 * language it already ships.
 *
 * Its Shell descriptor matches only the `sh`/`ksh`/`bash` extensions and
 * `PKGBUILD`, so the shell files people actually edit — `.envrc`, the
 * rc/profile family — arrive with no grammar at all. These entries widen
 * the *match* only: the grammar still loads on demand down the same path.
 *
 * Consulted after language-data's own lookup, so it can never shadow
 * something upstream already knows.
 */
const EXTRA_FILENAMES: [RegExp, string][] = [
  [
    /^\.?(envrc|bashrc|bash_profile|bash_login|bash_logout|bash_aliases|profile|kshrc|zshrc|zshenv|zprofile|zlogin|zlogout|zsh_aliases|inputrc)$/,
    "Shell",
  ],
  [/\.(zsh|ash|dash|mksh|bats|ebuild|eclass)$/, "Shell"],
  // Compose files and CI configs are YAML with a name that hides it.
  [/^\.?(yamllint|clang-format|clang-tidy)$/, "YAML"],
];

/** The descriptor for a path, by upstream match then by the table above. */
function descFor(path: string): LanguageDescription | null {
  // Callers pass whole paths (the diff and commit views do); the filename
  // patterns are anchored, so match on the last segment.
  const name = path.slice(path.lastIndexOf("/") + 1);
  const byName = LanguageDescription.matchFilename(ALL_LANGUAGES, name);
  if (byName) return byName;
  for (const [pattern, lang] of EXTRA_FILENAMES) {
    if (pattern.test(name)) {
      const desc = LanguageDescription.matchLanguageName(ALL_LANGUAGES, lang);
      if (desc) return desc;
    }
  }
  return null;
}

const loaded = new Map<string, LanguageSupport>();
const failed = new Set<string>();
const inflight = new Set<string>();
// Ticks whenever a grammar finishes loading, so reactive callers of
// `langForFile` re-run. `equals: false` — every load is a distinct event.
const [langLoads, bumpLangLoads] = createSignal(0, { equals: false });

function beginLoad(desc: LanguageDescription): void {
  if (loaded.has(desc.name) || failed.has(desc.name) || inflight.has(desc.name))
    return;
  inflight.add(desc.name);
  desc.load().then(
    (support) => {
      loaded.set(desc.name, support);
      inflight.delete(desc.name);
      bumpLangLoads(0);
    },
    () => {
      failed.add(desc.name);
      inflight.delete(desc.name);
    },
  );
}

/** LanguageSupport for a file, or null until it loads. Kicks off the load and,
 *  being reactive on an internal signal, re-runs its caller once ready. */
export function langForFile(name: string): LanguageSupport | null {
  langLoads(); // track: re-run when any grammar finishes loading
  const desc = descFor(name);
  if (!desc) return null;
  const support = loaded.get(desc.name);
  if (support) return support;
  beginLoad(desc);
  return null;
}

/** Await the LanguageSupport for a file (null if none matches or it fails). */
export async function loadLangForFile(
  name: string,
): Promise<LanguageSupport | null> {
  const desc = descFor(name);
  if (!desc) return null;
  const cached = loaded.get(desc.name);
  if (cached) return cached;
  if (failed.has(desc.name)) return null;
  try {
    const support = await desc.load();
    loaded.set(desc.name, support);
    bumpLangLoads(0);
    return support;
  } catch {
    failed.add(desc.name);
    return null;
  }
}
