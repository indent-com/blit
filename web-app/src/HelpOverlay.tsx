import { ui } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";

export function HelpOverlay({
  onClose,
  dark,
}: {
  onClose: () => void;
  dark: boolean;
}) {
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
    <OverlayBackdrop dark={dark} label="Help" onClose={onClose}>
      <OverlayPanel dark={dark} style={{ minWidth: 300 }}>
        <OverlayHeader
          dark={dark}
          title="Keyboard shortcuts"
          onClose={onClose}
        />
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
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
