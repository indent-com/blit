import type { Theme } from "./theme";

/** Mermaid is large and most views never draw a diagram, so load it lazily. */
let mermaidModule: Promise<typeof import("mermaid")> | null = null;
function loadMermaid(): Promise<typeof import("mermaid")> {
  mermaidModule ??= import("mermaid");
  return mermaidModule;
}

/** Parse an `rgb()`/`rgba()` string into components; null if unrecognised. */
function parseRgb(value: string): [number, number, number] | null {
  const match = /rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)/.exec(value);
  return match ? [Number(match[1]), Number(match[2]), Number(match[3])] : null;
}

/** Blend `color` toward `toward`, returning an opaque colour. */
function blend(color: string, toward: string, amount: number): string {
  const from = parseRgb(color);
  const to = parseRgb(toward);
  if (!from || !to) return color;
  const mixed = from.map((value, index) => {
    const destination = to[index] ?? value;
    return Math.round(value + (destination - value) * amount);
  });
  return `rgb(${mixed[0]}, ${mixed[1]}, ${mixed[2]})`;
}

/** Rough relative luminance, for Mermaid's light/dark switch. */
function luminance(color: string): number {
  const components = parseRgb(color);
  if (!components) return 0;
  const [red, green, blue] = components.map((value) => value / 255);
  return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
}

/** Mermaid theme variables derived from the active terminal palette. */
function mermaidVars(theme: Theme): Record<string, string> {
  const { bg, fg, accent, success, warning, error } = theme;
  const fill = (hue: string) => blend(hue, bg, 0.78);
  const soft = (hue: string) => blend(hue, bg, 0.88);
  const hues = [accent, success, warning, error];
  const series: Record<string, string> = {};
  for (let index = 0; index < 8; index++) {
    const hue = hues[index % hues.length];
    const color = index < hues.length ? hue : blend(hue, fg, 0.35);
    series[`pie${index + 1}`] = color;
    series[`git${index}`] = color;
  }
  return {
    darkMode: String(!!parseRgb(bg) && luminance(bg) < 0.5),
    background: bg,
    fontFamily: "inherit",
    primaryColor: fill(accent),
    primaryBorderColor: accent,
    primaryTextColor: fg,
    secondaryColor: fill(success),
    secondaryBorderColor: success,
    secondaryTextColor: fg,
    tertiaryColor: fill(warning),
    tertiaryBorderColor: warning,
    tertiaryTextColor: fg,
    lineColor: blend(accent, fg, 0.25),
    textColor: fg,
    mainBkg: fill(accent),
    nodeBorder: accent,
    nodeTextColor: fg,
    clusterBkg: soft(accent),
    clusterBorder: blend(accent, bg, 0.55),
    titleColor: fg,
    edgeLabelBackground: bg,
    actorBkg: fill(accent),
    actorBorder: accent,
    actorTextColor: fg,
    actorLineColor: blend(fg, bg, 0.5),
    signalColor: blend(accent, fg, 0.25),
    signalTextColor: fg,
    labelBoxBkgColor: fill(success),
    labelBoxBorderColor: success,
    labelTextColor: fg,
    loopTextColor: fg,
    activationBkgColor: fill(warning),
    activationBorderColor: warning,
    noteBkgColor: fill(warning),
    noteBorderColor: warning,
    noteTextColor: fg,
    sequenceNumberColor: bg,
    errorBkgColor: fill(error),
    errorTextColor: fg,
    ...series,
  };
}

// Mermaid has global configuration and uses temporary DOM IDs while rendering.
// Serialize calls so two independently mounted diagrams cannot reconfigure it
// underneath one another.
let renderQueue: Promise<void> = Promise.resolve();

export function renderMermaid(
  id: string,
  source: string,
  theme: Theme,
  options: { useMaxWidth?: boolean } = {},
): Promise<string> {
  const run = async (): Promise<string> => {
    const { default: mermaid } = await loadMermaid();
    mermaid.initialize({
      startOnLoad: false,
      theme: "base",
      securityLevel: "strict",
      themeVariables: mermaidVars(theme),
      flowchart: { useMaxWidth: options.useMaxWidth ?? true },
    });
    return (await mermaid.render(id, source)).svg;
  };
  const result = renderQueue.then(run, run);
  renderQueue = result.then(
    () => undefined,
    () => undefined,
  );
  return result;
}
