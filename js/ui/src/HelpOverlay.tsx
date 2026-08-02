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
  const isMac = /Mac|iPhone|iPad/.test(navigator.platform);
  const mod = isMac ? "Cmd" : "Ctrl";
  // CodeMirror binds different chords per platform for these two.
  const fold = isMac ? "Cmd+Alt+[ / ]" : "Ctrl+Shift+[ / ]";
  const undoRedo = isMac
    ? "Cmd+Z / Cmd+Shift+Z"
    : "Ctrl+Z / Ctrl+Y / Ctrl+Shift+Z";
  // Sections are hand-dealt between the two columns to keep their
  // heights close; re-deal when a section grows.
  const left: Section[] = [
    {
      title: t("help.keyboard"),
      items: [
        [`${mod}+K`, t("help.menu")],
        [`${mod}+Enter`, t("help.newTerminal")],
        [`${mod}+Shift+Enter`, t("help.newTerminalHere")],
        [`Ctrl+Shift+Q`, t("help.removeFromPane")],
        [`Ctrl+Alt+Shift+Q`, t("help.closeTerminal")],
        ["Alt+Shift+[ / ]", t("help.prevNextTerminal")],
        ["Ctrl+[ / ]", t("help.prevNextPane")],
        ["Ctrl+Shift+V", t("help.paste")],
        ["Ctrl+Shift+E", t("help.dockExplorer")],
        ["Ctrl+Shift+F", t("help.projectSearch")],
        ["Ctrl+Shift+L", t("help.dockLog")],
        ["Ctrl+Shift+P", t("help.dockProblems")],
        ["Ctrl+Shift+B", t("help.previewPanel")],
        ["Ctrl+Shift+K", t("help.soloPane")],
        ["Ctrl+Shift+O", "Open a URL as a web pane"],
        ["Ctrl+Shift+`", t("help.debugPanel")],
        ["Ctrl+Shift+A", t("help.resetAudio")],
        ["Ctrl+?", t("help.thisHelp")],
        ["Escape", t("help.closeOverlay")],
      ],
    },
    {
      // The Cmd+K field is a mode switcher, not just a filter — the
      // prefixes are invisible unless something says so.
      title: t("help.searchModes"),
      items: [
        ["name", t("help.modePlain")],
        [">command", t("help.modeCommand")],
        ["target>command", t("help.modeTargetCommand")],
        ["@file", t("help.modeFile")],
        ["#symbol", t("help.modeSymbol")],
      ],
    },
    {
      title: t("help.scrollback"),
      items: [
        ["Shift+Wheel", t("help.scroll")],
        ["Shift+PageUp / PageDown", t("help.pageUpDown")],
        ["Shift+Home / End", t("help.topBottom")],
        ["Any key", t("help.exitScrollback")],
      ],
    },
  ];
  const right: Section[] = [
    {
      title: t("help.editor"),
      items: [
        [`F12 / ${mod}+Click`, t("help.goToDef")],
        ["Shift+F12", t("help.findRefs")],
        [t("help.hoverPointer"), t("help.hover")],
        ["F2", t("help.rename")],
        [`${mod}+Shift+O`, t("help.outline")],
        ["F8 / Shift+F8", t("help.nextDiagnostic")],
        [`${mod}+Shift+M`, t("help.listDiagnostics")],
        ["Ctrl+Alt+← / →", t("help.navBackForward")],
        ["Ctrl+Space", t("help.completion")],
        ["Tab / Enter", t("help.acceptCompletion")],
        ["( / ,", t("help.signatureHelp")],
      ],
    },
    {
      title: t("help.editing"),
      items: [
        [`${mod}+S`, t("help.saveFile")],
        [undoRedo, t("help.undoRedo")],
        [`${mod}+/`, t("help.toggleComment")],
        ["Alt+↑ / ↓", t("help.moveLine")],
        ["Shift+Alt+↑ / ↓", t("help.copyLine")],
        ["Alt+Z", t("help.softWrap")],
        [fold, t("help.fold")],
        ["Alt+Click", t("help.addCursor")],
        ["Alt+Shift+drag", t("help.columnSelect")],
      ],
    },
    {
      title: t("help.find"),
      items: [
        [`${mod}+F`, t("help.findInFile")],
        [`F3 / Shift+F3`, t("help.findNextPrev")],
        [`${mod}+D`, t("help.selectNextOccurrence")],
        [`${mod}+Shift+L`, t("help.selectAllOccurrences")],
        [`${mod}+Alt+G`, t("help.gotoLine")],
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
    {
      title: t("help.touch"),
      items: [
        ["Swipe", t("help.touchScroll")],
        ["Long-press + drag", t("help.touchSelectCopy")],
        ["Toolbar Paste", t("help.touchPaste")],
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
