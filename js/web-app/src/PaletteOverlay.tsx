import { useState, useCallback, useEffect, useRef } from "react";
import { PALETTES } from "blit-react";
import type { TerminalPalette } from "blit-react";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t, tp } from "./i18n";

type PaletteTone = "dark" | "light";

export function PaletteOverlay({
  current,
  fontSize,
  onSelect,
  onPreview,
  onClose,
}: {
  current: TerminalPalette;
  fontSize: number;
  onSelect: (p: TerminalPalette) => void;
  onPreview: (p: TerminalPalette) => void;
  onClose: () => void;
}) {
  const theme = themeFor(current);
  const scale = uiScale(fontSize);
  const originalRef = useRef(current);
  const [tone, setTone] = useState<PaletteTone>(
    originalRef.current.dark ? "dark" : "light",
  );
  const [query, setQuery] = useState("");
  const tonePalettes = PALETTES.filter((palette) =>
    tone === "dark" ? palette.dark : !palette.dark,
  );
  const toneInitialIdx = tonePalettes.findIndex(
    (palette) => palette.id === originalRef.current.id,
  );
  const [selectedIdx, setSelectedIdx] = useState(
    toneInitialIdx >= 0 ? toneInitialIdx : 0,
  );
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);

  const dismiss = useCallback(() => {
    onPreview(originalRef.current);
    onClose();
  }, [onPreview, onClose]);

  const trimmedQuery = query.trim();
  const showAllPalettes = trimmedQuery.length === 0;
  const lowerQuery = trimmedQuery.toLowerCase();
  const filtered = showAllPalettes
    ? tonePalettes
    : tonePalettes.filter(
        (palette) =>
          palette.name.toLowerCase().includes(lowerQuery) ||
          palette.id.toLowerCase().includes(lowerQuery),
      );

  const preview = useCallback(
    (idx: number) => {
      const palette = filtered[idx];
      if (!palette) return;
      setSelectedIdx(idx);
      onPreview(palette);
    },
    [filtered, onPreview],
  );

  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  useEffect(() => {
    const el = listRef.current?.children[selectedIdx] as
      | HTMLElement
      | undefined;
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx, filtered]);

  useEffect(() => {
    if (showAllPalettes && toneInitialIdx >= 0) {
      setSelectedIdx(toneInitialIdx);
    } else if (filtered.length > 0) {
      setSelectedIdx(0);
    } else {
      setSelectedIdx(-1);
    }
  }, [filtered.length, showAllPalettes, toneInitialIdx]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          if (filtered.length > 0) {
            preview((selectedIdx + 1 + filtered.length) % filtered.length);
          }
          break;
        case "ArrowUp":
          e.preventDefault();
          if (filtered.length > 0) {
            preview((selectedIdx - 1 + filtered.length) % filtered.length);
          }
          break;
        case "Enter":
          if (filtered.length > 0) {
            e.preventDefault();
            const palette =
              selectedIdx >= 0 && selectedIdx < filtered.length
                ? filtered[selectedIdx]
                : filtered[0];
            onSelect(palette);
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
    },
    [dismiss, filtered, onSelect, preview, selectedIdx],
  );

  const inputStyle = {
    ...ui.input,
    backgroundColor: theme.inputBg,
    color: "inherit",
    fontSize: scale.md,
  };

  return (
    <OverlayBackdrop
      palette={current}
      label={t("palette.label")}
      onClose={dismiss}
    >
      <OverlayPanel palette={current} fontSize={fontSize}>
        <OverlayHeader
          palette={current}
          fontSize={fontSize}
          title={t("palette.title")}
          onClose={dismiss}
        />
        <div
          style={{ display: "flex", flexDirection: "column", gap: scale.gap }}
        >
          <div
            role="tablist"
            aria-label={t("palette.toneLabel")}
            style={{ display: "flex", gap: scale.tightGap + 2 }}
          >
            {[
              { id: "dark" as const, label: t("palette.dark") },
              { id: "light" as const, label: t("palette.light") },
            ].map((option) => {
              const active = tone === option.id;
              return (
                <button
                  key={option.id}
                  type="button"
                  role="tab"
                  aria-selected={active}
                  onMouseDown={(event) => event.preventDefault()}
                  onClick={() => {
                    setTone(option.id);
                    inputRef.current?.focus();
                  }}
                  style={{
                    ...ui.btn,
                    padding: `${scale.controlY}px ${scale.controlX + 2}px`,
                    border: `1px solid ${active ? theme.border : "transparent"}`,
                    backgroundColor: active ? theme.selectedBg : "transparent",
                    opacity: active ? 1 : 0.7,
                    fontSize: scale.sm,
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
              alignItems: "center",
              gap: scale.tightGap + 2,
              fontSize: scale.sm,
              opacity: 0.65,
            }}
          >
            <span>{t("palette.tabHintPress")}</span>
            <kbd style={{ ...ui.kbd, fontSize: scale.sm }}>{"Tab"}</kbd>
            <span>{t("palette.tabHintSuffix")}</span>
          </div>
          <input
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={handleKeyDown}
            placeholder={tp("palette.searchPlaceholder", {
              tone: t(`palette.${tone}`),
            })}
            autoComplete="off"
            autoCorrect="off"
            autoCapitalize="off"
            spellCheck={false}
            style={inputStyle}
          />
          <ul
            ref={listRef}
            style={{
              margin: 0,
              padding: 0,
              listStyle: "none",
              outline: "none",
              maxHeight: "20em",
              overflow: "auto",
            }}
          >
            {filtered.map((p, i) => (
              <li key={p.id} style={{ listStyle: "none" }}>
                <button
                  onClick={() => onSelect(p)}
                  onMouseEnter={() => preview(i)}
                  style={{
                    display: "flex",
                    alignItems: "center",
                    gap: scale.gap + 2,
                    padding: `${scale.controlY + 2}px ${scale.controlX}px`,
                    border: "none",
                    fontFamily: "inherit",
                    cursor: "pointer",
                    width: "100%",
                    color: "inherit",
                    textAlign: "left" as const,
                    backgroundColor:
                      i === selectedIdx ? theme.selectedBg : "transparent",
                    fontSize: scale.md,
                  }}
                >
                  <span style={{ display: "flex", gap: 2 }}>
                    <span
                      style={{
                        ...ui.swatch,
                        backgroundColor: `rgb(${p.bg[0]},${p.bg[1]},${p.bg[2]})`,
                        border: `1px solid ${theme.subtleBorder}`,
                      }}
                    />
                    <span
                      style={{
                        ...ui.swatch,
                        backgroundColor: `rgb(${p.fg[0]},${p.fg[1]},${p.fg[2]})`,
                      }}
                    />
                    {p.ansi.slice(0, 8).map((c, j) => (
                      <span
                        key={j}
                        style={{
                          ...ui.swatch,
                          backgroundColor: `rgb(${c[0]},${c[1]},${c[2]})`,
                        }}
                      />
                    ))}
                  </span>
                  <span style={{ fontSize: scale.md }}>{p.name}</span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
