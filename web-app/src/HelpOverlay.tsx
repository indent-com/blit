import type { TerminalPalette } from "blit-react";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t } from "./i18n";

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
  const left: Section[] = [
    {
      title: t("help.keyboard"),
      items: [
        [`${mod}+K`, t("help.menu")],
        [`${mod}+Shift+Enter`, t("help.newTerminal")],
        [`${mod}+Shift+W`, t("help.closeTerminal")],
        [`${mod}+Shift+{ / }`, t("help.prevNextTerminal")],
        ["Ctrl+[ / ]", t("help.prevNextPane")],
        ["Ctrl+Shift+`", t("help.debugPanel")],
        ["Ctrl+?", t("help.thisHelp")],
        ["Escape", t("help.closeOverlay")],
      ],
    },
  ];
  const right: Section[] = [
    {
      title: t("help.scrollback"),
      items: [
        ["Shift+Wheel", t("help.scroll")],
        ["Ctrl+PageUp / PageDown", t("help.pageUpDown")],
        ["Ctrl+Home / End", t("help.topBottom")],
        ["Any key", t("help.exitScrollback")],
      ],
    },
    {
      title: t("help.mouse"),
      items: [
        ["Click + drag", t("help.selectText")],
        ["Double / Triple-click", t("help.selectWordLine")],
        ["Alt+Click", t("help.openUrl")],
        ["Scrollbar", t("help.dragScroll")],
      ],
    },
  ];

  return (
    <OverlayBackdrop palette={palette} label={t("help.label")} onClose={onClose}>
      <OverlayPanel palette={palette} fontSize={fontSize}>
        <OverlayHeader palette={palette} fontSize={fontSize} title={t("help.title")} onClose={onClose} />
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
