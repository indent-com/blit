import { createSignal, createEffect, onMount, Show, For } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t } from "./i18n";

export function FontOverlay(props: {
  currentFamily: string;
  currentSize: number;
  serverFonts: string[];
  palette: TerminalPalette;
  fontSize: number;
  onSelect: (font: string, size: number) => void;
  onPreview: (font: string, size: number) => void;
  onClose: () => void;
}) {
  const theme = themeFor(props.palette);
  const scale = uiScale(props.fontSize);
  const originalFamily = props.currentFamily;
  const originalSize = props.currentSize;
  const initialFamily = originalFamily.trim();
  const initialFamilyLower = initialFamily.toLowerCase();
  const initialIdx = props.serverFonts.findIndex(
    (font) => font.toLowerCase() === initialFamilyLower,
  );

  const [query, setQuery] = createSignal(initialFamily);
  const [size, setSize] = createSignal(props.currentSize);
  const [selectedIdx, setSelectedIdx] = createSignal(initialIdx);
  const [hoverIdx, setHoverIdx] = createSignal(-1);

  let inputRef!: HTMLInputElement;
  let listRef!: HTMLUListElement;

  const trimmedQuery = () => query().trim();
  const showAllFonts = () =>
    trimmedQuery().length === 0 ||
    trimmedQuery().toLowerCase() === initialFamilyLower;
  const lowerQuery = () => trimmedQuery().toLowerCase();

  const filtered = () => {
    if (showAllFonts()) return props.serverFonts;
    const q = lowerQuery();
    return props.serverFonts.filter((f) => f.toLowerCase().includes(q));
  };

  const dismiss = () => {
    props.onPreview(originalFamily, originalSize);
    props.onClose();
  };

  const previewFont = (family: string) => {
    props.onPreview(family, size());
  };

  const selectFont = (idx: number) => {
    const f = filtered();
    setSelectedIdx(idx);
    setHoverIdx(-1);
    if (idx >= 0 && idx < f.length) {
      setQuery(f[idx]);
      previewFont(f[idx]);
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
      case "Escape":
        e.preventDefault();
        dismiss();
        break;
    }
  };

  onMount(() => {
    inputRef?.focus();
    inputRef?.select();
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
    if (showAllFonts()) {
      setSelectedIdx(initialIdx);
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
            const f = filtered();
            const idx = selectedIdx();
            const family =
              idx >= 0 && idx < f.length
                ? f[idx]
                : trimmedQuery() || originalFamily;
            props.onSelect(family, size());
          }}
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: `${scale.gap}px`,
            flex: 1,
            "min-height": 0,
          }}
        >
          <input
            ref={inputRef!}
            type="text"
            value={query()}
            onInput={(e) => setQuery(e.currentTarget.value)}
            onKeyDown={handleKeyDown}
            placeholder={t("font.placeholder")}
            autocomplete="off"
            autocorrect="off"
            autocapitalize="off"
            spellcheck={false}
            style={inputStyle()}
          />
          <Show when={filtered().length > 0}>
            <ul
              ref={listRef!}
              style={{
                margin: 0,
                padding: 0,
                overflow: "auto",
                flex: 1,
                "min-height": 0,
                "max-height": "20em",
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
                    {f}
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
              type="range"
              min={8}
              max={32}
              value={size()}
              onInput={(e) => setSize(Number(e.currentTarget.value))}
              style={{ flex: 1 }}
            />
            <input
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
