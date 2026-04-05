import { For } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { t } from "./i18n";

type Shortcut = [string, string];
type Section = { title: string; items: Shortcut[] };

export function HelpOverlay(props: {
  onClose: () => void;
  palette: TerminalPalette;
  fontSize: number;
}) {
  const theme = themeFor(props.palette);
  const scale = uiScale(props.fontSize);
  const mod = /Mac|iPhone|iPad/.test(navigator.platform) ? "Cmd" : "Ctrl";
  const left: Section[] = [
    {
      title: t("help.keyboard"),
      items: [
        [`${mod}+K`, t("help.menu")],
        [`${mod}+Shift+Enter`, t("help.newTerminal")],
        [`${mod}+Shift+Q`, t("help.closeTerminal")],
        [`${mod}+Shift+{ / }`, t("help.prevNextTerminal")],
        ["Ctrl+[ / ]", t("help.prevNextPane")],
        ["Ctrl+Shift+`", t("help.debugPanel")],
        ["Ctrl+Shift+B", t("help.previewPanel")],
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
        ["Shift+PageUp / PageDown", t("help.pageUpDown")],
        ["Shift+Home / End", t("help.topBottom")],
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
    <OverlayBackdrop
      palette={props.palette}
      label={t("help.label")}
      onClose={props.onClose}
    >
      <OverlayPanel palette={props.palette} fontSize={props.fontSize}>
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={t("help.title")}
          onClose={props.onClose}
        />
        <div
          style={{
            display: "flex",
            gap: `${scale.gap * 3}px`,
            padding: `${scale.tightGap}px 0`,
          }}
        >
          <Column sections={left} theme={theme} scale={scale} />
          <Column sections={right} theme={theme} scale={scale} />
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}

function Column(props: {
  sections: Section[];
  theme: ReturnType<typeof themeFor>;
  scale: ReturnType<typeof uiScale>;
}) {
  return (
    <div style={{ flex: 1, "min-width": 0 }}>
      <For each={props.sections}>
        {(s) => (
          <div style={{ "margin-bottom": `${props.scale.gap * 2}px` }}>
            <div
              style={{
                "font-size": `${props.scale.sm}px`,
                "font-weight": 600,
                color: props.theme.dimFg,
                "margin-bottom": `${props.scale.tightGap}px`,
                "text-transform": "uppercase",
                "letter-spacing": "0.05em",
              }}
            >
              {s.title}
            </div>
            <table
              style={{
                "border-spacing": `${props.scale.controlX}px ${props.scale.controlY}px`,
                "margin-left": `${-props.scale.controlX}px`,
              }}
            >
              <tbody>
                <For each={s.items}>
                  {([key, desc]) => (
                    <tr>
                      <td style={{ "white-space": "nowrap" }}>
                        <kbd
                          style={{
                            ...ui.kbd,
                            "font-size": `${props.scale.sm}px`,
                          }}
                        >
                          {key}
                        </kbd>
                      </td>
                      <td
                        style={{
                          "font-size": `${props.scale.md}px`,
                          color: props.theme.dimFg,
                        }}
                      >
                        {desc}
                      </td>
                    </tr>
                  )}
                </For>
              </tbody>
            </table>
          </div>
        )}
      </For>
    </div>
  );
}
