import type { TerminalPalette } from "blit-react";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";

type Shortcut = [string, string];
type Section = { title: string; items: Shortcut[] };

export function HelpOverlay({
  onClose,
  palette,
  fontSize,
}: {
  onClose: () => void;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = themeFor(palette);
  const scale = uiScale(fontSize);
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const sections: Section[] = [
    {
      title: "Terminal",
      items: [
        [`${mod}+K`, "Workspace switcher (layouts, panes, terminals, actions)"],
        [`${mod}+Shift+Enter`, "New terminal in cwd"],
        [`${mod}+Shift+W`, "Close terminal"],
        [`${mod}+Shift+{ / }`, "Prev / Next terminal"],
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
      <OverlayPanel palette={palette} fontSize={fontSize} style={{ minWidth: 520, maxWidth: 680 }}>
        <OverlayHeader palette={palette} fontSize={fontSize} title="Keyboard & mouse shortcuts" onClose={onClose} />
        <div style={{ display: "flex", gap: scale.gap * 3, padding: `${scale.tightGap}px 0` }}>
          <Column sections={left} theme={theme} scale={scale} />
          <Column sections={right} theme={theme} scale={scale} />
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}

function Column({
  sections,
  theme,
  scale,
}: {
  sections: Section[];
  theme: ReturnType<typeof themeFor>;
  scale: ReturnType<typeof uiScale>;
}) {
  return (
    <div style={{ flex: 1, minWidth: 0 }}>
      {sections.map((s) => (
        <div key={s.title} style={{ marginBottom: scale.gap * 2 }}>
          <div
            style={{
              fontSize: scale.sm,
              fontWeight: 600,
              color: theme.dimFg,
              marginBottom: scale.tightGap,
              textTransform: "uppercase",
              letterSpacing: "0.05em",
            }}
          >
            {s.title}
          </div>
          <table style={{ borderSpacing: `${scale.controlX}px ${scale.controlY}px`, marginLeft: -scale.controlX }}>
            <tbody>
              {s.items.map(([key, desc]) => (
                <tr key={key}>
                  <td style={{ whiteSpace: "nowrap" }}>
                    <kbd style={{ ...ui.kbd, fontSize: scale.sm }}>{key}</kbd>
                  </td>
                  <td style={{ fontSize: scale.md, color: theme.dimFg }}>{desc}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
    </div>
  );
}
