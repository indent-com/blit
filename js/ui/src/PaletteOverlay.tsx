import { createSignal, createEffect, onMount, For } from "solid-js";
import { PALETTES } from "@blit-sh/core";
import type { TerminalPalette } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t, tp } from "./i18n";

type PaletteTone = "dark" | "light";

export function PaletteOverlay(props: {
  current: TerminalPalette;
  fontSize: number;
  onSelect: (p: TerminalPalette) => void;
  onPreview: (p: TerminalPalette) => void;
  onClose: () => void;
}) {
  const original = props.current;
  const theme = themeFor(props.current);
  const scale = uiScale(props.fontSize);
  const [tone, setTone] = createSignal<PaletteTone>(
    original.dark ? "dark" : "light",
  );
  const [query, setQuery] = createSignal("");
  const [selectedIdx, setSelectedIdx] = createSignal(0);

  let inputRef!: HTMLInputElement;
  let listRef!: HTMLUListElement;

  const tonePalettes = () =>
    PALETTES.filter((palette) =>
      tone() === "dark" ? palette.dark : !palette.dark,
    );

  const toneInitialIdx = () =>
    tonePalettes().findIndex((palette) => palette.id === original.id);

  const trimmedQuery = () => query().trim();
  const showAllPalettes = () => trimmedQuery().length === 0;
  const lowerQuery = () => trimmedQuery().toLowerCase();

  const filtered = () => {
    const all = tonePalettes();
    if (showAllPalettes()) return all;
    const q = lowerQuery();
    return all.filter(
      (palette) =>
        palette.name.toLowerCase().includes(q) ||
        palette.id.toLowerCase().includes(q),
    );
  };

  const dismiss = () => {
    props.onPreview(original);
    props.onClose();
  };

  const preview = (idx: number) => {
    const palette = filtered()[idx];
    if (!palette) return;
    setSelectedIdx(idx);
    props.onPreview(palette);
  };

  onMount(() => {
    inputRef?.focus();
  });

  // Scroll selected item into view
  createEffect(() => {
    const idx = selectedIdx();
    const f = filtered();
    void f.length; // track filtered
    const el = listRef?.children[idx] as HTMLElement | undefined;
    el?.scrollIntoView({ block: "nearest" });
  });

  // Reset selection when filter or tone changes
  createEffect(() => {
    const initIdx = toneInitialIdx();
    if (showAllPalettes() && initIdx >= 0) {
      setSelectedIdx(initIdx);
    } else if (filtered().length > 0) {
      setSelectedIdx(0);
    } else {
      setSelectedIdx(-1);
    }
  });

  const handleKeyDown = (e: KeyboardEvent) => {
    const f = filtered();
    const idx = selectedIdx();
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        if (f.length > 0) {
          preview((idx + 1 + f.length) % f.length);
        }
        break;
      case "ArrowUp":
        e.preventDefault();
        if (f.length > 0) {
          preview((idx - 1 + f.length) % f.length);
        }
        break;
      case "Enter":
        if (f.length > 0) {
          e.preventDefault();
          const palette = idx >= 0 && idx < f.length ? f[idx] : f[0];
          props.onSelect(palette);
        }
        break;
      case "Tab":
        e.preventDefault();
        setTone((value) => (value === "dark" ? "light" : "dark"));
        break;
      case "Escape":
        e.preventDefault();
        dismiss();
        break;
    }
  };

  const inputStyle = () => ({
    ...ui.input,
    "background-color": theme.inputBg,
    color: "inherit",
    "font-size": `${scale.md}px`,
  });

  return (
    <OverlayBackdrop
      palette={props.current}
      label={t("palette.label")}
      onClose={dismiss}
    >
      <OverlayPanel palette={props.current} fontSize={props.fontSize}>
        <OverlayHeader
          palette={props.current}
          fontSize={props.fontSize}
          title={t("palette.title")}
          onClose={dismiss}
        />
        <div
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: `${scale.gap}px`,
          }}
        >
          <div
            role="tablist"
            aria-label={t("palette.toneLabel")}
            style={{ display: "flex", gap: `${scale.tightGap + 2}px` }}
          >
            {[
              { id: "dark" as const, label: t("palette.dark") },
              { id: "light" as const, label: t("palette.light") },
            ].map((option) => {
              const active = () => tone() === option.id;
              return (
                <button
                  type="button"
                  role="tab"
                  aria-selected={active()}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => {
                    setTone(option.id);
                    inputRef?.focus();
                  }}
                  style={{
                    ...ui.btn,
                    padding: `${scale.controlY}px ${scale.controlX + 2}px`,
                    border: `1px solid ${active() ? theme.border : "transparent"}`,
                    "background-color": active()
                      ? theme.selectedBg
                      : "transparent",
                    opacity: active() ? 1 : 0.7,
                    "font-size": `${scale.sm}px`,
                  }}
                >
                  {option.label}
                </button>
              );
            })}
          </div>
          <div
            style={{
              display: "flex",
              "align-items": "center",
              gap: `${scale.tightGap + 2}px`,
              "font-size": `${scale.sm}px`,
              opacity: 0.65,
            }}
          >
            <span>{t("palette.tabHintPress")}</span>
            <kbd style={{ ...ui.kbd, "font-size": `${scale.sm}px` }}>
              {"Tab"}
            </kbd>
            <span>{t("palette.tabHintSuffix")}</span>
          </div>
          <input
            ref={inputRef!}
            type="text"
            value={query()}
            onInput={(e) => setQuery(e.currentTarget.value)}
            onKeyDown={handleKeyDown}
            placeholder={tp("palette.searchPlaceholder", {
              tone: t(`palette.${tone()}`),
            })}
            autocomplete="off"
            autocorrect="off"
            autocapitalize="off"
            spellcheck={false}
            style={inputStyle()}
          />
          <ul
            ref={listRef!}
            style={{
              margin: 0,
              padding: 0,
              "list-style": "none",
              outline: "none",
              "max-height": "20em",
              overflow: "auto",
            }}
          >
            <For each={filtered()}>
              {(p, i) => (
                <li style={{ "list-style": "none" }}>
                  <button
                    onClick={() => props.onSelect(p)}
                    onMouseEnter={() => preview(i())}
                    style={{
                      display: "flex",
                      "align-items": "center",
                      gap: `${scale.gap + 2}px`,
                      padding: `${scale.controlY + 2}px ${scale.controlX}px`,
                      border: "none",
                      "font-family": "inherit",
                      cursor: "pointer",
                      width: "100%",
                      color: "inherit",
                      "text-align": "left",
                      "background-color":
                        i() === selectedIdx()
                          ? theme.selectedBg
                          : "transparent",
                      "font-size": `${scale.md}px`,
                    }}
                  >
                    <span style={{ display: "flex", gap: "2px" }}>
                      <span
                        style={{
                          ...ui.swatch,
                          "background-color": `rgb(${p.bg[0]},${p.bg[1]},${p.bg[2]})`,
                          border: `1px solid ${theme.subtleBorder}`,
                        }}
                      />
                      <span
                        style={{
                          ...ui.swatch,
                          "background-color": `rgb(${p.fg[0]},${p.fg[1]},${p.fg[2]})`,
                        }}
                      />
                      <For each={p.ansi.slice(0, 8)}>
                        {(c) => (
                          <span
                            style={{
                              ...ui.swatch,
                              "background-color": `rgb(${c[0]},${c[1]},${c[2]})`,
                            }}
                          />
                        )}
                      </For>
                    </span>
                    <span style={{ "font-size": `${scale.md}px` }}>
                      {p.name}
                    </span>
                  </button>
                </li>
              )}
            </For>
          </ul>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
