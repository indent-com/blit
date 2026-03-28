import {
  useState,
  useEffect,
  useRef,
  useCallback,
} from "react";
import { DEFAULT_FONT, type TerminalPalette } from "blit-react";
import { themeFor, ui } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";

export function FontOverlay({
  currentFamily,
  currentSize,
  serverFonts,
  palette,
  onSelect,
  onPreview,
  onClose,
}: {
  currentFamily: string;
  currentSize: number;
  serverFonts: string[];
  palette: TerminalPalette;
  onSelect: (font: string, size: number) => void;
  onPreview: (font: string, size: number) => void;
  onClose: () => void;
}) {
  const theme = themeFor(palette);
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
  };

  const previewFamily = selectedIdx >= 0 && selectedIdx < filtered.length
    ? filtered[selectedIdx]
    : trimmedQuery || originalRef.current.family;

  return (
    <OverlayBackdrop palette={palette} label="Font" onClose={dismiss}>
      <OverlayPanel
        palette={palette}
        style={{
          minWidth: 320,
          display: "flex",
          flexDirection: "column",
        }}
      >
        <OverlayHeader palette={palette} title="Font" onClose={dismiss} />
        <form onSubmit={(e) => {
          e.preventDefault();
          const family = selectedIdx >= 0 && selectedIdx < filtered.length
            ? filtered[selectedIdx]
            : trimmedQuery || originalRef.current.family;
          onSelect(family, size);
        }} style={{ display: "flex", flexDirection: "column", gap: 8, flex: 1, minHeight: 0 }}>
        <input
          ref={inputRef}
          type="text"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          onKeyDown={handleKeyDown}
          placeholder="Search fonts or type a name"
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
              maxHeight: 200,
            }}
          >
            {filtered.map((f, i) => (
              <li
                key={f}
                style={{
                  padding: "4px 8px",
                  cursor: "pointer",
                  backgroundColor: i === selectedIdx
                    ? theme.selectedBg
                    : "transparent",
                  listStyle: "none",
                  fontSize: 13,
                }}
                onClick={() => onSelect(f, size)}
                onMouseEnter={() => { setSelectedIdx(i); previewFont(f); }}
              >
                {f}
              </li>
            ))}
          </ul>
        )}
        <div style={{ display: "flex", alignItems: "center", gap: 8, flexShrink: 0 }}>
          <label style={{ fontSize: 13, opacity: 0.7, flexShrink: 0 }}>Size</label>
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
            style={{ ...inputStyle, width: 52, flex: "none", textAlign: "center" }}
          />
        </div>
        <span style={{ fontSize: size, fontFamily: previewFamily || DEFAULT_FONT, flexShrink: 0 }}>
          The quick brown fox
        </span>
        <button type="submit" style={{
          ...ui.btn,
          alignSelf: "flex-end",
          padding: "4px 12px",
          border: `1px solid ${theme.subtleBorder}`,
          backgroundColor: theme.inputBg,
          flexShrink: 0,
        }}>Apply</button>
        </form>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
