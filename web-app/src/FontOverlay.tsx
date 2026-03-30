import {
  useState,
  useEffect,
  useRef,
  useCallback,
} from "react";
import type { TerminalPalette } from "blit-react";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t } from "./i18n";

export function FontOverlay({
  currentFamily,
  currentSize,
  serverFonts,
  palette,
  fontSize,
  onSelect,
  onPreview,
  onClose,
}: {
  currentFamily: string;
  currentSize: number;
  serverFonts: string[];
  palette: TerminalPalette;
  fontSize: number;
  onSelect: (font: string, size: number) => void;
  onPreview: (font: string, size: number) => void;
  onClose: () => void;
}) {
  const theme = themeFor(palette);
  const scale = uiScale(fontSize);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLUListElement>(null);
  const originalRef = useRef({ family: currentFamily, size: currentSize });
  const initialFamily = originalRef.current.family.trim();
  const initialFamilyLower = initialFamily.toLowerCase();
  const initialIdx = serverFonts.findIndex(
    (font) => font.toLowerCase() === initialFamilyLower,
  );
  const [query, setQuery] = useState(initialFamily);
  const [size, setSize] = useState(currentSize);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx);

  const dismiss = useCallback(() => {
    onPreview(originalRef.current.family, originalRef.current.size);
    onClose();
  }, [onPreview, onClose]);

  useEffect(() => {
    inputRef.current?.focus();
    inputRef.current?.select();
  }, []);

  const trimmedQuery = query.trim();
  const showAllFonts =
    trimmedQuery.length === 0 || trimmedQuery.toLowerCase() === initialFamilyLower;
  const lowerQuery = trimmedQuery.toLowerCase();
  const filtered = showAllFonts
    ? serverFonts
    : serverFonts.filter((f) => f.toLowerCase().includes(lowerQuery));

  // Preview the selected font in the terminal
  const previewFont = useCallback((family: string) => {
    onPreview(family, size);
  }, [onPreview, size]);

  // Keyboard navigation
  const handleKeyDown = useCallback((e: React.KeyboardEvent) => {
    switch (e.key) {
      case "ArrowDown":
        e.preventDefault();
        setSelectedIdx((i) => Math.min(i + 1, filtered.length - 1));
        break;
      case "ArrowUp":
        e.preventDefault();
        setSelectedIdx((i) => Math.max(i - 1, -1));
        break;
      case "Escape":
        e.preventDefault();
        dismiss();
        break;
    }
  }, [filtered, selectedIdx, query, size, onSelect, dismiss]);

  // Preview on keyboard selection change
  useEffect(() => {
    if (selectedIdx >= 0 && selectedIdx < filtered.length) {
      previewFont(filtered[selectedIdx]);
      const el = listRef.current?.children[selectedIdx] as HTMLElement | undefined;
      el?.scrollIntoView({ block: "nearest" });
    }
  }, [selectedIdx, filtered, previewFont]);

  // Reset selection when query changes
  useEffect(() => {
    if (showAllFonts) {
      setSelectedIdx(initialIdx);
    } else {
      setSelectedIdx(-1);
    }
  }, [initialIdx, showAllFonts]);

  const inputStyle = {
    ...ui.input,
    backgroundColor: theme.inputBg,
    color: "inherit",
    fontSize: scale.md,
  };

  return (
    <OverlayBackdrop palette={palette} label={t("font.label")} onClose={dismiss}>
      <OverlayPanel
        palette={palette}
        fontSize={fontSize}
        style={{
          display: "flex",
          flexDirection: "column",
        }}
      >
        <OverlayHeader palette={palette} fontSize={fontSize} title={t("font.title")} onClose={dismiss} />
        <form onSubmit={(e) => {
          e.preventDefault();
          const family = selectedIdx >= 0 && selectedIdx < filtered.length
            ? filtered[selectedIdx]
            : trimmedQuery || originalRef.current.family;
          onSelect(family, size);
        }} style={{ display: "flex", flexDirection: "column", gap: scale.gap, flex: 1, minHeight: 0 }}>
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder={t("font.placeholder")}
          autoComplete="off"
          autoCorrect="off"
          autoCapitalize="off"
          spellCheck={false}
          style={inputStyle}
        />
        {filtered.length > 0 && (
          <ul
            ref={listRef}
            style={{
              margin: 0,
              padding: 0,
              overflow: "auto",
              flex: 1,
              minHeight: 0,
              maxHeight: "20em",
            }}
          >
            {filtered.map((f, i) => (
              <li
                key={f}
                style={{
                  padding: `${scale.controlY}px ${scale.controlX}px`,
                  cursor: "pointer",
                  backgroundColor: i === selectedIdx
                    ? theme.selectedBg
                    : "transparent",
                  listStyle: "none",
                  fontSize: scale.md,
                }}
                onClick={() => onSelect(f, size)}
                onMouseEnter={() => { setSelectedIdx(i); previewFont(f); }}
              >
                {f}
              </li>
            ))}
          </ul>
        )}
        <div style={{ display: "flex", alignItems: "center", gap: scale.gap, flexShrink: 0 }}>
          <label style={{ fontSize: scale.md, opacity: 0.7, flexShrink: 0 }}>{t("font.sizeLabel")}</label>
          <input
            type="range"
            min={8}
            max={32}
            value={size}
            onChange={(e) => setSize(Number(e.target.value))}
            style={{ flex: 1 }}
          />
          <input
            type="number"
            min={6}
            max={72}
            value={size}
            onChange={(e) => {
              const n = Number(e.target.value);
              if (n > 0) setSize(n);
            }}
            style={{ ...inputStyle, width: "3.5em", flex: "none", textAlign: "center" }}
          />
        </div>
        <button type="submit" style={{
          ...ui.btn,
          alignSelf: "flex-end",
          padding: `${scale.controlY}px ${scale.controlX + 4}px`,
          border: `1px solid ${theme.subtleBorder}`,
          backgroundColor: theme.inputBg,
          fontSize: scale.sm,
          flexShrink: 0,
        }}>{t("font.apply")}</button>
        </form>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
