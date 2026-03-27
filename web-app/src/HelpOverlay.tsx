import { themeFor, layout, ui } from "./theme";

export function HelpOverlay({
  onClose,
  dark,
}: {
  onClose: () => void;
  dark: boolean;
}) {
  const theme = themeFor(dark);
  const mod_ = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const shortcuts = [
    [`${mod_}+K`, "Expose (switch PTYs)"],
    [`${mod_}+Shift+Enter`, "New PTY in cwd"],
    [`${mod_}+Shift+W`, "Close PTY"],
    [`${mod_}+Shift+{ / }`, "Prev / Next PTY"],
    ["Shift+PageUp/Down", "Scroll"],
    [`${mod_}+Shift+P`, "Palette picker"],
    [`${mod_}+Shift+F`, "Font picker"],
    ["Ctrl+?", "Help"],
  ];
  return (
    <div
      open
      style={layout.overlay}
      onClick={onClose}
    >
      <article
        style={{
          ...layout.panel,
          minWidth: 300,
          backgroundColor: theme.solidPanelBg,
          color: theme.fg,
        }}
        onClick={(e) => e.stopPropagation()}
      >
        <h2 style={{ fontWeight: 600, marginBottom: 12, fontSize: 16 }}>
          Keyboard shortcuts
        </h2>
        <table style={{ borderSpacing: "12px 6px" }}>
          <tbody>
            {shortcuts.map(([key, desc]) => (
              <tr key={key}>
                <td>
                  <kbd style={ui.kbd}>{key}</kbd>
                </td>
                <td style={{ fontSize: 13 }}>{desc}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </article>
    </div>
  );
}
