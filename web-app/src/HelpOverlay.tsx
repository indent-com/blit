import type { TerminalPalette } from "blit-react";
import { themeFor, ui } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";

type Shortcut = [string, string];
type Section = { title: string; items: Shortcut[] };

export function HelpOverlay({
  onClose,
  palette,
}: {
  onClose: () => void;
  palette: TerminalPalette;
}) {
  const theme = themeFor(palette);
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const sections: Section[] = [
    {
      title: "Terminal",
      items: [
        [`${mod}+K`, "Expose (switch PTYs)"],
        [`${mod}+Shift+Enter`, "New PTY in cwd"],
        [`${mod}+Shift+W`, "Close PTY"],
        [`${mod}+Shift+{ / }`, "Prev / Next PTY"],
        ["Escape", "Close overlay"],
      ],
    },
    {
      title: "Scrollback",
      items: [
        ["Shift+Wheel", "Scroll (even in mouse mode)"],
        ["Ctrl+PageUp", "Page up"],
        ["Ctrl+PageDown", "Page down"],
        ["Ctrl+Home", "Top of scrollback"],
        ["Ctrl+End", "Bottom (live view)"],
        ["Any key", "Exit scrollback"],
      ],
    },
    {
      title: "Selection",
      items: [
        ["Click + drag", "Select text"],
        ["Double-click", "Select word"],
        ["Triple-click", "Select line"],
      ],
    },
    {
      title: "Mouse",
      items: [
        ["Alt+Click", "Open URL"],
        ["Scrollbar drag", "Scroll scrollback"],
        ["Scrollbar track click", "Jump to position"],
      ],
    },
    {
      title: "Appearance",
      items: [
        [`${mod}+Shift+P`, "Palette picker"],
        [`${mod}+Shift+F`, "Font picker"],
        ["Ctrl+Shift+`", "Debug panel"],
        ["Ctrl+?", "This help"],
      ],
    },
  ];

  const half = Math.ceil(sections.length / 2);
  const left = sections.slice(0, half);
  const right = sections.slice(half);

  return (
    <OverlayBackdrop palette={palette} label="Help" onClose={onClose}>
      <OverlayPanel palette={palette} style={{ minWidth: 520, maxWidth: 680 }}>
        <OverlayHeader palette={palette} title="Keyboard & mouse shortcuts" onClose={onClose} />
        <div style={{ display: "flex", gap: 24, padding: "4px 0" }}>
          <Column sections={left} theme={theme} />
          <Column sections={right} theme={theme} />
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}

function Column({
  sections,
  theme,
}: {
  sections: Section[];
  theme: ReturnType<typeof themeFor>;
}) {
  return (
    <div style={{ flex: 1, minWidth: 0 }}>
      {sections.map((s) => (
        <div key={s.title} style={{ marginBottom: 12 }}>
          <div
            style={{
              fontSize: 11,
              fontWeight: 600,
              color: theme.dimFg,
              marginBottom: 4,
              textTransform: "uppercase",
              letterSpacing: "0.05em",
            }}
          >
            {s.title}
          </div>
          <table style={{ borderSpacing: "8px 3px", marginLeft: -8 }}>
            <tbody>
              {s.items.map(([key, desc]) => (
                <tr key={key}>
                  <td style={{ whiteSpace: "nowrap" }}>
                    <kbd style={ui.kbd}>{key}</kbd>
                  </td>
                  <td style={{ fontSize: 12, color: theme.dimFg }}>{desc}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
    </div>
  );
}
