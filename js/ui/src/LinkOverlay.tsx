import { Show } from "solid-js";
import type { TerminalPalette, UrlAssessment } from "@blit-sh/core";

import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import { overlayChromeStyles, themeFor, ui, uiScale } from "./theme";
import { t } from "./i18n";

/**
 * Confirmation dialog for a hyperlink the classifier would not open outright.
 *
 * Two jobs, in order of importance:
 *
 * 1. Show the user where the click actually goes. For an OSC 8 link the text on
 *    screen is chosen independently of the target, so the destination is the
 *    only thing worth reading — it is rendered in a monospace block, and always
 *    from `assessment.display`, whose invisible and text-reordering codepoints
 *    have been escaped to `<U+XXXX>`. Rendering `assessment.raw` here would
 *    reintroduce exactly the deception the escaping exists to defeat.
 * 2. Make refusing the default. A `deny` verdict offers no way to proceed, and
 *    a `confirm` puts the cancel action first in the tab order.
 */
export function LinkOverlay(props: {
  palette: TerminalPalette;
  fontSize?: number;
  assessment: UrlAssessment;
  /** On-screen text of the link, when it differs from the destination. */
  linkText?: string;
  onOpen: () => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize ?? 13);
  const styles = () =>
    overlayChromeStyles(theme(), props.palette.dark, scale());

  const blocked = () => props.assessment.verdict === "deny";
  const tone = () => (blocked() ? theme().errorText : theme().warning);

  // `detail` from core is English-only; the reason code is what gets
  // translated. Falling back to `detail` keeps a new reason code from
  // rendering as a bare key.
  const explanation = () => {
    const key = `link.reason.${props.assessment.reason}`;
    const translated = t(key);
    return translated === key ? props.assessment.detail : translated;
  };

  /**
   * Worth warning about only when the visible text is itself URL-shaped: text
   * that merely labels a link ("the docs") is normal markup, whereas text that
   * looks like an address while pointing somewhere else is the actual attack.
   */
  const misleading = () => {
    const text = props.linkText?.trim();
    if (!text || text.length < 4) return false;
    if (
      !/^[a-z][a-z0-9+.-]*:\/\//i.test(text) &&
      !/^[\w-]+\.[a-z]{2,}/i.test(text)
    )
      return false;
    return !props.assessment.raw.startsWith(text);
  };

  const hasEscapes = () => props.assessment.display.includes("<U+");

  const blockStyle = () => ({
    ...ui.input,
    display: "block",
    width: "100%",
    "box-sizing": "border-box" as const,
    "font-family": "monospace",
    "font-size": `${scale().sm}px`,
    "background-color": theme().inputBg,
    border: `1px solid ${theme().subtleBorder}`,
    color: theme().fg,
    padding: `${scale().controlY}px ${scale().controlX}px`,
    "border-radius": "0",
    // A long target must wrap rather than be truncated: an ellipsis is a place
    // for the real destination to hide.
    "overflow-wrap": "anywhere" as const,
    "white-space": "pre-wrap" as const,
    "max-height": `${scale().md * 9}px`,
    "overflow-y": "auto" as const,
    // Pin visual order to byte order. Escaping strips bidi *controls*, but RTL
    // letters would still be reordered by the bidi algorithm — and a dialog
    // that reorders the destination it is asking you to approve is worse than
    // no dialog at all.
    direction: "ltr" as const,
    "unicode-bidi": "bidi-override" as const,
  });

  /**
   * Built as whole objects rather than `{...base(), key: value}` at the call
   * site: Solid's compiler splits a style object into its static keys and its
   * dynamic spread, applies the static keys first, and then lets the spread
   * overwrite them — so an inline override of a key the base already sets is
   * silently discarded.
   */
  const destinationStyle = () => ({
    ...blockStyle(),
    "border-color": tone(),
  });

  const footerStyle = () => ({
    ...styles().footer,
    "justify-content": "flex-end",
  });

  const openButtonStyle = () => ({
    ...styles().actionButton,
    "border-radius": "0",
    border: `1px solid ${theme().accent}`,
    "background-color": theme().accent,
    color: "#fff",
  });

  const cancelButtonStyle = () => ({
    ...styles().actionButton,
    "border-radius": "0",
  });

  const labelStyle = () => ({
    "font-size": `${scale().xs}px`,
    opacity: 0.7,
    "margin-bottom": `${scale().tightGap}px`,
    "text-transform": "uppercase" as const,
    "letter-spacing": "0.06em",
  });

  return (
    <OverlayBackdrop
      palette={props.palette}
      label={t("link.label")}
      onClose={props.onClose}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{ "max-width": "min(640px, 92vw)" }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title={
            <span style={{ color: tone() }}>
              {blocked() ? t("link.blockedTitle") : t("link.confirmTitle")}
            </span>
          }
          subtitle={explanation()}
          onClose={props.onClose}
        />

        <div
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: `${scale().gap}px`,
            padding: `${scale().tightGap}px 0`,
          }}
        >
          <Show when={misleading()}>
            <p
              style={{
                margin: "0",
                color: tone(),
                "font-size": `${scale().sm}px`,
              }}
            >
              {t("link.mismatch")}
            </p>
          </Show>

          <Show when={misleading() && props.linkText}>
            {(text) => (
              <div>
                <div style={labelStyle()}>{t("link.linkText")}</div>
                <code style={blockStyle()}>{text()}</code>
              </div>
            )}
          </Show>

          <div>
            <div style={labelStyle()}>{t("link.destination")}</div>
            {/* Always `display`, never `raw`. */}
            <code style={destinationStyle()}>{props.assessment.display}</code>
          </div>

          <Show when={hasEscapes()}>
            <p
              style={{
                margin: "0",
                opacity: 0.7,
                "font-size": `${scale().xs}px`,
              }}
            >
              {t("link.escapedNote")}
            </p>
          </Show>
        </div>

        <div style={footerStyle()}>
          <button
            type="button"
            style={cancelButtonStyle()}
            onClick={props.onClose}
          >
            {blocked() ? t("link.dismiss") : t("link.cancel")}
          </button>
          {/* No affordance at all for a denied target — there is no wording of
              a button that makes opening one safe. */}
          <Show when={!blocked()}>
            <button
              type="button"
              style={openButtonStyle()}
              onClick={props.onOpen}
            >
              {t("link.open")}
            </button>
          </Show>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
