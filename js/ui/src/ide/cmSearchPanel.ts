/**
 * The editor's find/replace panel, in place of CodeMirror's.
 *
 * CodeMirror's own panel is styled as bare browser chrome — native
 * checkboxes, gradient buttons — and its markup is a flat row of eleven
 * controls with a `<br>` in the middle. It cannot be made to match the
 * project-search pane (ide/SearchPanel) from CSS alone: a `<br>` measures
 * zero width as a flex item however it is styled, and any flex-basis large
 * enough to break the row at one window width fits at another. So this
 * supplies the DOM instead, and keeps CodeMirror's search *state* — the
 * query effect, the cursor, the commands — untouched.
 *
 * The result shares the project search's vocabulary: a query field that
 * grows, `Aa` / `.*` / `ab|` toggle chips, a match counter, and replace on
 * its own row only when asked for. Colours come from cmTheme's
 * `.blit-cm-search-*` rules, so the panel re-themes with everything else.
 */

import type { EditorView, Panel } from "@codemirror/view";
import {
  SearchQuery,
  closeSearchPanel,
  findNext,
  findPrevious,
  getSearchQuery,
  replaceAll,
  replaceNext,
  selectMatches,
  setSearchQuery,
} from "@codemirror/search";
import { t } from "../i18n";

/** Stop counting past this: the number stops being informative long before
 *  it stops being expensive, and a huge minified file should not stall the
 *  editor for a label. */
const COUNT_CAP = 5000;

/** How many matches the query has, and which one holds the cursor. */
function tally(view: EditorView): { index: number; total: number } {
  const query = getSearchQuery(view.state);
  if (!query.search) return { index: 0, total: 0 };
  const head = view.state.selection.main.from;
  let total = 0;
  let index = 0;
  try {
    const cursor = query.getCursor(view.state.doc);
    for (;;) {
      const next = cursor.next();
      if (next.done || total >= COUNT_CAP) break;
      total++;
      if (next.value.from <= head && head <= next.value.to) index = total;
    }
  } catch {
    // An invalid regex mid-typing throws rather than matching nothing.
    return { index: 0, total: -1 };
  }
  return { index, total };
}

export function blitSearchPanel(view: EditorView): Panel {
  const initial = getSearchQuery(view.state);

  const dom = document.createElement("div");
  dom.className = "blit-cm-search";
  dom.addEventListener("keydown", (event) => {
    // The panel's own keys must not reach the global shortcut handler or
    // the editor beneath it.
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      closeSearchPanel(view);
      view.focus();
      return;
    }
    if (event.key === "Enter") {
      event.preventDefault();
      event.stopPropagation();
      if (event.target === replaceField) replaceNext(view);
      else if (event.shiftKey) findPrevious(view);
      else findNext(view);
      return;
    }
    event.stopPropagation();
  });

  const row = (): HTMLDivElement => {
    const el = document.createElement("div");
    el.className = "blit-cm-search-row";
    dom.appendChild(el);
    return el;
  };
  const findRow = row();
  const replaceRow = row();

  const field = (placeholder: string, value: string): HTMLInputElement => {
    const el = document.createElement("input");
    el.className = "blit-cm-search-field";
    el.type = "text";
    el.placeholder = placeholder;
    el.value = value;
    el.spellcheck = false;
    el.setAttribute("autocapitalize", "off");
    return el;
  };
  const searchField = field(t("editorSearch.find"), initial.search);
  const replaceField = field(t("editorSearch.replace"), initial.replace);

  /** A flat on/off chip, the same control the project-search pane uses. */
  const chip = (
    label: string,
    title: string,
    pressed: boolean,
    onClick: () => void,
  ): HTMLButtonElement => {
    const el = document.createElement("button");
    el.className = "blit-cm-search-chip";
    el.type = "button";
    el.textContent = label;
    el.title = title;
    // aria-pressed is both the accessible state and the style hook, so the
    // two can never disagree.
    el.setAttribute("aria-pressed", String(pressed));
    el.addEventListener("click", (e) => {
      e.preventDefault();
      onClick();
    });
    return el;
  };

  /** A plain action button (prev/next/replace). */
  const action = (
    label: string,
    title: string,
    onClick: () => void,
  ): HTMLButtonElement => {
    const el = document.createElement("button");
    el.className = "blit-cm-search-action";
    el.type = "button";
    el.textContent = label;
    el.title = title;
    el.addEventListener("click", (e) => {
      e.preventDefault();
      onClick();
      view.focus();
    });
    return el;
  };

  let caseSensitive = initial.caseSensitive;
  let regexp = initial.regexp;
  let wholeWord = initial.wholeWord;
  let replaceOpen = initial.replace !== "";

  function commit(): void {
    view.dispatch({
      effects: setSearchQuery.of(
        new SearchQuery({
          search: searchField.value,
          caseSensitive,
          regexp,
          wholeWord,
          replace: replaceField.value,
        }),
      ),
    });
  }

  const count = document.createElement("span");
  count.className = "blit-cm-search-count";

  const caseChip = chip(
    "Aa",
    t("editorSearch.matchCase"),
    caseSensitive,
    () => {
      caseSensitive = !caseSensitive;
      caseChip.setAttribute("aria-pressed", String(caseSensitive));
      commit();
    },
  );
  const regexpChip = chip(".*", t("editorSearch.regexp"), regexp, () => {
    regexp = !regexp;
    regexpChip.setAttribute("aria-pressed", String(regexp));
    commit();
  });
  const wordChip = chip("ab|", t("editorSearch.wholeWord"), wholeWord, () => {
    wholeWord = !wholeWord;
    wordChip.setAttribute("aria-pressed", String(wholeWord));
    commit();
  });

  function syncReplaceRow(): void {
    replaceRow.style.display = replaceOpen ? "" : "none";
    replaceChip.setAttribute("aria-pressed", String(replaceOpen));
  }
  const replaceChip = chip(
    "⇄",
    t("editorSearch.toggleReplace"),
    replaceOpen,
    () => {
      replaceOpen = !replaceOpen;
      syncReplaceRow();
      if (replaceOpen) replaceField.focus();
    },
  );

  searchField.addEventListener("input", commit);
  replaceField.addEventListener("input", commit);

  findRow.append(
    searchField,
    count,
    action("↑", t("editorSearch.previous"), () => findPrevious(view)),
    action("↓", t("editorSearch.next"), () => findNext(view)),
    action("⧉", t("editorSearch.selectAll"), () => selectMatches(view)),
    caseChip,
    regexpChip,
    wordChip,
    replaceChip,
  );
  const closeButton = action("✕", t("editorSearch.close"), () =>
    closeSearchPanel(view),
  );
  // add, not assign: it must keep the action styling as well.
  closeButton.classList.add("blit-cm-search-close");
  findRow.append(closeButton);

  replaceRow.append(
    replaceField,
    action("replace", t("editorSearch.replaceNext"), () => replaceNext(view)),
    action("all", t("editorSearch.replaceAll"), () => replaceAll(view)),
  );
  syncReplaceRow();

  function refresh(): void {
    const { index, total } = tally(view);
    if (total < 0) {
      count.textContent = t("editorSearch.badRegex");
      count.dataset.state = "error";
    } else if (!searchField.value) {
      count.textContent = "";
      delete count.dataset.state;
    } else if (total === 0) {
      count.textContent = t("editorSearch.noMatches");
      count.dataset.state = "empty";
    } else {
      const shown = total >= COUNT_CAP ? `${COUNT_CAP}+` : String(total);
      count.textContent = index > 0 ? `${index}/${shown}` : shown;
      delete count.dataset.state;
    }
  }

  return {
    dom,
    top: true,
    mount() {
      refresh();
      searchField.focus();
      searchField.select();
    },
    update(update) {
      // Another command can change the query — Mod-d extending a selection,
      // or a restored panel — so the fields follow the state, not the other
      // way round.
      if (
        update.docChanged ||
        update.selectionSet ||
        update.transactions.length
      )
        refresh();
      const q = getSearchQuery(update.state);
      if (
        q.search !== searchField.value &&
        document.activeElement !== searchField
      )
        searchField.value = q.search;
      if (q.caseSensitive !== caseSensitive) {
        caseSensitive = q.caseSensitive;
        caseChip.setAttribute("aria-pressed", String(caseSensitive));
      }
      if (q.regexp !== regexp) {
        regexp = q.regexp;
        regexpChip.setAttribute("aria-pressed", String(regexp));
      }
      if (q.wholeWord !== wholeWord) {
        wholeWord = q.wholeWord;
        wordChip.setAttribute("aria-pressed", String(wholeWord));
      }
    },
  };
}
