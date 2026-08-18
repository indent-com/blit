/**
 * The small shared vocabulary the sections under an expanded remote row are
 * built from — headings, status pills, rows, and buttons.
 *
 * It exists because those sections are read together: applications and clients
 * sit one above the other under the same remote, so a heading that is bold in
 * one and dim in the other, or two different ideas of what a row's padding is,
 * reads as two panels bolted together rather than one view of a remote. Keeping
 * the primitives here is what makes adding a third section cheap and
 * consistent rather than another round of copied styles.
 *
 * Everything takes the resolved theme and scale rather than reading them, so
 * these stay pure and the caller keeps one source of truth for both.
 */

import type { JSX } from "solid-js";
import { Show } from "solid-js";
import type { Theme, UIScale } from "./theme";
import { ui } from "./theme";

/** Semantic status, mapped to a colour by {@link pillColor}. */
export type PanelTone = "ok" | "warn" | "bad" | "idle";

export function pillColor(theme: Theme, tone: PanelTone): string {
  switch (tone) {
    case "ok":
      return theme.accent;
    case "warn":
      return theme.errorText;
    case "bad":
      return theme.error;
    case "idle":
      return theme.dimFg;
  }
}

/**
 * A section label with its count, and room for a control on the right.
 *
 * The count is part of the heading rather than a separate line because the
 * first question about any of these lists is "how many", and answering it in
 * the heading means a collapsed-looking empty section still says so.
 */
export function SectionHeading(props: {
  theme: Theme;
  scale: UIScale;
  label: string;
  count?: number;
  children?: JSX.Element;
}): JSX.Element {
  return (
    <div
      style={{
        display: "flex",
        "align-items": "center",
        "justify-content": "space-between",
        gap: `${props.scale.gap}px`,
        padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
      }}
    >
      <span
        style={{
          display: "flex",
          "align-items": "baseline",
          gap: `${props.scale.tightGap}px`,
          "font-size": `${props.scale.sm}px`,
          "text-transform": "uppercase",
          "letter-spacing": "0.08em",
          color: props.theme.dimFg,
        }}
      >
        {props.label}
        <Show when={props.count !== undefined}>
          <span style={{ "font-variant-numeric": "tabular-nums" }}>
            {props.count}
          </span>
        </Show>
      </span>
      {props.children}
    </div>
  );
}

/**
 * A dot and a word: the status of one row, readable without colour.
 *
 * The dot carries the colour and the word carries the meaning, so this stays
 * legible to a viewer who cannot separate the hues — which matters here
 * because "running" and "backoff" are otherwise the same short word shape.
 */
export function StatusPill(props: {
  theme: Theme;
  scale: UIScale;
  tone: PanelTone;
  label: string;
  title?: string;
}): JSX.Element {
  return (
    <span
      title={props.title}
      style={{
        display: "inline-flex",
        "align-items": "center",
        gap: `${props.scale.tightGap}px`,
        "font-size": `${props.scale.sm}px`,
        color: props.theme.dimFg,
        "white-space": "nowrap",
      }}
    >
      <span
        aria-hidden="true"
        style={{
          width: "0.5em",
          height: "0.5em",
          "border-radius": "50%",
          "background-color": pillColor(props.theme, props.tone),
          "flex-shrink": "0",
        }}
      />
      {props.label}
    </span>
  );
}

/** One row of a section, with the shared padding and separator. */
export function PanelRow(props: {
  theme: Theme;
  scale: UIScale;
  children: JSX.Element;
}): JSX.Element {
  return (
    <article
      style={{
        padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
        "border-bottom": `1px solid ${props.theme.subtleBorder}`,
        display: "grid",
        gap: `${props.scale.tightGap}px`,
      }}
    >
      {props.children}
    </article>
  );
}

/** The button style both sections use. `tone` tints a destructive or armed
 *  action without changing its shape, so the row does not reflow on arming. */
export function panelButton(
  theme: Theme,
  scale: UIScale,
  tone?: PanelTone,
): JSX.CSSProperties {
  return {
    ...ui.btn,
    color: tone ? pillColor(theme, tone) : "inherit",
    "background-color": "transparent",
    border: `1px solid ${tone ? pillColor(theme, tone) : theme.border}`,
    "border-radius": "0",
    "font-size": `${scale.sm}px`,
    padding: `${scale.controlY}px ${scale.controlX}px`,
    cursor: "pointer",
    "white-space": "nowrap",
  };
}

/** The muted line a section shows when it has nothing to list. */
export function PanelEmpty(props: {
  theme: Theme;
  scale: UIScale;
  children: JSX.Element;
}): JSX.Element {
  return (
    <p
      style={{
        margin: "0",
        padding: `${props.scale.controlX}px`,
        color: props.theme.dimFg,
        "font-size": `${props.scale.sm}px`,
      }}
    >
      {props.children}
    </p>
  );
}
