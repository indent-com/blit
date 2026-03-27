import {
  useState,
  useCallback,
  useEffect,
  useRef,
} from "react";
import { PALETTES } from "blit-react";
import type { TerminalPalette } from "blit-react";
import { themeFor, layout, ui } from "./theme";

export function PaletteOverlay({
  current,
  onSelect,
  onPreview,
  onClose,
  dark,
}: {
  current: TerminalPalette;
  onSelect: (p: TerminalPalette) => void;
  onPreview: (p: TerminalPalette) => void;
  onClose: () => void;
  dark: boolean;
}) {
  const theme = themeFor(dark);
  const originalRef = useRef(current);
  const initialIdx = PALETTES.findIndex((p) => p.id === current.id);
  const [selectedIdx, setSelectedIdx] = useState(initialIdx >= 0 ? initialIdx : 0);
  const listRef = useRef<HTMLMenuElement>(null);

  const dismiss = useCallback(() => {
    onPreview(originalRef.current);
    onClose();
  }, [onPreview, onClose]);

  const preview = useCallback((idx: number) => {
    setSelectedIdx(idx);
    onPreview(PALETTES[idx]);
  }, [onPreview]);

  useEffect(() => {
    listRef.current?.focus();
  }, []);

  useEffect(() => {
    const el = listRef.current?.children[selectedIdx + 1] as HTMLElement | undefined;
    el?.scrollIntoView({ block: "nearest" });
  }, [selectedIdx]);

  const handleKeyDown = useCallback(
    (e: React.KeyboardEvent) => {
      switch (e.key) {
        case "ArrowDown":
          e.preventDefault();
          preview((selectedIdx + 1) % PALETTES.length);
          break;
        case "ArrowUp":
          e.preventDefault();
          preview((selectedIdx - 1 + PALETTES.length) % PALETTES.length);
          break;
        case "Enter":
          e.preventDefault();
          onSelect(PALETTES[selectedIdx]);
          break;
        case "Escape":
          e.preventDefault();
          dismiss();
          break;
      }
    },
    [selectedIdx, onSelect, preview, dismiss],
  );

  return (
    <div
      open
      style={layout.overlay}
      onClick={dismiss}
    >
      <menu
        ref={listRef}
        tabIndex={0}
        onKeyDown={handleKeyDown}
        style={{
          ...layout.panel,
          minWidth: 280,
          listStyle: "none",
          backgroundColor: theme.solidPanelBg,
          color: theme.fg,
          outline: "none",
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <li style={{ fontWeight: 600, marginBottom: 8, listStyle: "none" }}>Palette</li>
        {PALETTES.map((p, i) => (
          <li key={p.id} style={{ listStyle: "none" }}>
            <button
              onClick={() => onSelect(p)}
              onMouseEnter={() => preview(i)}
              style={{
                display: "flex",
                alignItems: "center",
                gap: 10,
                padding: "6px 8px",
                border: "none",
                fontFamily: "inherit",
                cursor: "pointer",
                width: "100%",
                color: "inherit",
                textAlign: "left" as const,
                backgroundColor:
                  i === selectedIdx
                    ? theme.selectedBg
                    : "transparent",
              }}
            >
              <span style={{ display: "flex", gap: 2 }}>
                <span
                  style={{
                    ...ui.swatch,
                    backgroundColor: `rgb(${p.bg[0]},${p.bg[1]},${p.bg[2]})`,
                    border: "1px solid rgba(128,128,128,0.3)",
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
              <span style={{ fontSize: 13 }}>{p.name}</span>
            </button>
          </li>
        ))}
      </menu>
    </div>
  );
}
