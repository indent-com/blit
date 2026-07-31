import { createSignal, createEffect, onMount, Show, For } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { scrollbarStyle, themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t } from "./i18n";
import type { FontChoice } from "./fontCatalog";

export function FontOverlay(props: {
  currentFamily: string;
  currentSize: number;
  currentGamma: number;
  serverFonts: string[];
  /** Faces a host bundled into the page (see fontCatalog). When present these
   *  are the only choices, and the family box is gone: on a host with no
   *  `font/<family>` route, a typed name loads nothing. */
  fontChoices?: readonly FontChoice[];
  palette: TerminalPalette;
  fontSize: number;
  onSelect: (font: string, size: number, gamma: number) => void;
  onPreview: (font: string, size: number, gamma: number) => void;
  onClose: () => void;
}) {
  const theme = themeFor(props.palette);
  const scale = uiScale(props.fontSize);
  const originalFamily = props.currentFamily;
  const originalSize = props.currentSize;
  const originalGamma = props.currentGamma;
  const initialFamily = originalFamily.trim();
  const initialFamilyLower = initialFamily.toLowerCase();
  /** Choices come from the host, or from the server's installed families —
   *  where the family is its own label. */
  const catalog = () => props.fontChoices ?? [];
  const curated = () => catalog().length > 0;
  const choices = (): readonly FontChoice[] =>
    curated()
      ? catalog()
      : props.serverFonts.map((f) => ({ label: f, stack: f }));
  const initialIdx = () =>
    choices().findIndex((c) => c.stack.toLowerCase() === initialFamilyLower);

  const [query, setQuery] = createSignal(initialFamily);
  const [filterQuery, setFilterQuery] = createSignal("");
  const [size, setSize] = createSignal(props.currentSize);
  const [gamma, setGamma] = createSignal(props.currentGamma);
  const [selectedIdx, setSelectedIdx] = createSignal(initialIdx());
  const [hoverIdx, setHoverIdx] = createSignal(-1);

  let inputRef!: HTMLInputElement;
  let listRef!: HTMLUListElement;

  const trimmedQuery = () => query().trim();
  const trimmedFilter = () => filterQuery().trim();
  const showAllFonts = () => trimmedFilter().length === 0;
  const lowerFilter = () => trimmedFilter().toLowerCase();

  const filtered = (): readonly FontChoice[] => {
    if (curated() || showAllFonts()) return choices();
    const q = lowerFilter();
    return choices().filter((c) => c.label.toLowerCase().includes(q));
  };

  const dismiss = () => {
    props.onPreview(originalFamily, originalSize, originalGamma);
    props.onClose();
  };

  const previewFont = (family: string) => {
    props.onPreview(family, size(), gamma());
  };

  /** The family the form would apply right now: the highlighted list entry,
   *  else whatever has been typed, else what we opened with. */
  const pendingFamily = () => {
    const f = filtered();
    const idx = selectedIdx();
    if (idx >= 0 && idx < f.length) return f[idx].stack;
    // Curated hosts have no text box to fall back to.
    return (!curated() && trimmedQuery()) || originalFamily;
  };

  const selectFont = (idx: number) => {
    const f = filtered();
    setSelectedIdx(idx);
    setHoverIdx(-1);
    if (idx >= 0 && idx < f.length) {
      setQuery(f[idx].label);
      previewFont(f[idx].stack);
    }
  };

  const handleKeyDown = (e: KeyboardEvent) => {
    const f = filtered();
    const idx = selectedIdx();
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        selectFont(Math.min((idx < 0 ? -1 : idx) + 1, f.length - 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        selectFont(Math.max(idx - 1, 0));
        break;
      case "Enter":
        // Curated: the list holds the focus, and a list has no implicit form
        // submit the way the family box does — so Enter would do nothing at
        // all on the one control the picker has left.
        if (curated()) {
          e.preventDefault();
          props.onSelect(pendingFamily(), size(), gamma());
        }
        break;
      case "Escape":
        e.preventDefault();
        dismiss();
        break;
    }
  };

  onMount(() => {
    // Curated: the list is the whole control, so it takes the focus the
    // search box would have had, and arrow keys work on open.
    if (curated()) listRef?.focus();
    else {
      inputRef?.focus();
      inputRef?.select();
    }
  });

  // Scroll selected item into view
  createEffect(() => {
    const idx = selectedIdx();
    if (idx >= 0) {
      const el = listRef?.children[idx] as HTMLElement | undefined;
      el?.scrollIntoView({ block: "nearest" });
    }
  });

  // Reset selection when query changes from typing (not from selectFont)
  createEffect(() => {
    if (curated()) return;
    if (showAllFonts()) {
      setSelectedIdx(initialIdx());
    } else {
      setSelectedIdx(-1);
    }
  });

  const inputStyle = () => ({
    ...ui.input,
    "background-color": theme.inputBg,
    color: "inherit",
    "font-size": `${scale.md}px`,
  });

  return (
    <OverlayBackdrop
      palette={props.palette}
      label={t("font.label")}
      onClose={dismiss}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{
          display: "flex",
          "flex-direction": "column",
        }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={t("font.title")}
          onClose={dismiss}
        />
        <form
          onSubmit={(e) => {
            e.preventDefault();
            props.onSelect(pendingFamily(), size(), gamma());
          }}
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: `${scale.gap}px`,
            flex: 1,
            "min-height": 0,
          }}
        >
          <Show when={!curated()}>
            <input
              ref={inputRef!}
              name="blit-font-search"
              type="text"
              value={query()}
              onInput={(e) => {
                const v = e.currentTarget.value;
                setQuery(v);
                setFilterQuery(v);
              }}
              onKeyDown={handleKeyDown}
              placeholder={t("font.placeholder")}
              autocomplete="off"
              autocorrect="off"
              autocapitalize="off"
              spellcheck={false}
              style={inputStyle()}
            />
          </Show>
          <Show when={filtered().length > 0}>
            <ul
              ref={listRef!}
              tabindex={curated() ? 0 : undefined}
              onKeyDown={curated() ? handleKeyDown : undefined}
              style={{
                margin: 0,
                padding: 0,
                overflow: "auto",
                flex: 1,
                "min-height": 0,
                "max-height": "20em",
                ...scrollbarStyle(theme),
              }}
            >
              <For each={filtered()}>
                {(f, i) => (
                  <li
                    style={{
                      padding: `${scale.controlY}px ${scale.controlX}px`,
                      cursor: "pointer",
                      "background-color":
                        i() === selectedIdx()
                          ? theme.selectedBg
                          : i() === hoverIdx()
                            ? theme.hoverBg
                            : "transparent",
                      "list-style": "none",
                      "font-size": `${scale.md}px`,
                    }}
                    onClick={() => selectFont(i())}
                    onMouseEnter={() => setHoverIdx(i())}
                    onMouseLeave={() => setHoverIdx(-1)}
                  >
                    {f.label}
                  </li>
                )}
              </For>
            </ul>
          </Show>
          <div
            style={{
              display: "flex",
              "align-items": "center",
              gap: `${scale.gap}px`,
              "flex-shrink": 0,
            }}
          >
            <label
              style={{
                "font-size": `${scale.md}px`,
                opacity: 0.7,
                "flex-shrink": 0,
              }}
            >
              {t("font.sizeLabel")}
            </label>
            <input
              name="blit-font-size-range"
              type="range"
              min={8}
              max={32}
              value={size()}
              onInput={(e) => setSize(Number(e.currentTarget.value))}
              style={{ flex: 1 }}
            />
            <input
              name="blit-font-size"
              type="number"
              min={6}
              max={72}
              value={size()}
              onInput={(e) => {
                const n = Number(e.currentTarget.value);
                if (n > 0) setSize(n);
              }}
              style={{
                ...inputStyle(),
                width: "4.5em",
                flex: "none",
                "text-align": "center",
              }}
            />
          </div>
          <div
            style={{
              display: "flex",
              "align-items": "center",
              gap: `${scale.gap}px`,
              "flex-shrink": 0,
            }}
          >
            <label
              style={{
                "font-size": `${scale.md}px`,
                opacity: 0.7,
                "flex-shrink": 0,
              }}
              title={t("font.thinningHint")}
            >
              {t("font.thinningLabel")}
            </label>
            <input
              name="blit-text-gamma"
              type="range"
              min={0.5}
              max={2.5}
              step={0.05}
              value={gamma()}
              onInput={(e) => {
                const g = Number(e.currentTarget.value);
                setGamma(g);
                // Live: the whole point of this control is comparing the
                // rendered result, not a number.
                props.onPreview(pendingFamily(), size(), g);
              }}
              style={{ flex: 1 }}
            />
            <output
              style={{
                "font-size": `${scale.sm}px`,
                opacity: 0.7,
                width: "3em",
                "text-align": "center",
                "flex-shrink": 0,
              }}
            >
              {gamma().toFixed(2)}
            </output>
          </div>
          <button
            type="submit"
            style={{
              ...ui.btn,
              "align-self": "flex-end",
              padding: `${scale.controlY}px ${scale.controlX + 4}px`,
              border: `1px solid ${theme.subtleBorder}`,
              "background-color": theme.inputBg,
              "font-size": `${scale.sm}px`,
              "flex-shrink": 0,
            }}
          >
            {t("font.apply")}
          </button>
        </form>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
