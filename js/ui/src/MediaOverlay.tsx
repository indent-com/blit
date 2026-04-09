import { createSignal, Show, For, type JSX } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";

const AUDIO_PRESETS: { label: string; kbps: number }[] = [
  { label: "Default", kbps: 0 },
  { label: "32 kbps", kbps: 32 },
  { label: "64 kbps", kbps: 64 },
  { label: "96 kbps", kbps: 96 },
  { label: "128 kbps", kbps: 128 },
  { label: "192 kbps", kbps: 192 },
  { label: "256 kbps", kbps: 256 },
];

const VIDEO_PRESETS: { label: string; value: number }[] = [
  { label: "Default", value: 0 },
  { label: "Low", value: 1 },
  { label: "Medium", value: 2 },
  { label: "High", value: 3 },
  { label: "Lossless", value: 4 },
];

export function MediaOverlay(props: {
  palette: TerminalPalette;
  fontSize: number;
  audioBitrate: number;
  videoQuality: number;
  audioMuted: boolean;
  audioAvailable: boolean;
  onAudioBitrateChange: (kbps: number) => void;
  onVideoQualityChange: (quality: number) => void;
  onToggleAudio: () => void;
  onResetAudio: () => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  const [customBitrate, setCustomBitrate] = createSignal("");
  const isCustom = () =>
    props.audioBitrate > 0 &&
    !AUDIO_PRESETS.some((p) => p.kbps === props.audioBitrate);

  const sectionStyle = (): JSX.CSSProperties => ({
    display: "flex",
    "flex-direction": "column",
    gap: `${scale().tightGap}px`,
  });

  const labelStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.6,
    "text-transform": "uppercase",
    "letter-spacing": "0.05em",
  });

  const chipStyle = (active: boolean): JSX.CSSProperties => ({
    ...ui.btn,
    padding: `${scale().controlY}px ${scale().controlX + 2}px`,
    border: `1px solid ${active ? theme().border : "transparent"}`,
    "background-color": active ? theme().selectedBg : "transparent",
    opacity: active ? 1 : 0.7,
    "font-size": `${scale().sm}px`,
    cursor: "pointer",
  });

  const handleCustomBitrate = () => {
    const v = parseInt(customBitrate(), 10);
    if (v > 0 && v <= 65535) {
      props.onAudioBitrateChange(v);
      setCustomBitrate("");
    }
  };

  return (
    <OverlayBackdrop
      palette={props.palette}
      label="Media settings"
      onClose={props.onClose}
    >
      <OverlayPanel
        palette={props.palette}
        fontSize={props.fontSize}
        style={{ "min-width": "320px" }}
      >
        <OverlayHeader
          palette={props.palette}
          fontSize={props.fontSize}
          title="Media"
          onClose={props.onClose}
        />
        <div
          style={{
            display: "flex",
            "flex-direction": "column",
            gap: `${scale().gap + 4}px`,
          }}
        >
          {/* Audio section */}
          <div style={sectionStyle()}>
            <div
              style={{
                display: "flex",
                "align-items": "center",
                "justify-content": "space-between",
              }}
            >
              <span style={labelStyle()}>Audio</span>
              <Show when={props.audioAvailable}>
                <div
                  style={{
                    display: "flex",
                    gap: `${scale().tightGap}px`,
                    "align-items": "center",
                  }}
                >
                  <button
                    onClick={props.onToggleAudio}
                    style={{
                      ...ui.btn,
                      "font-size": `${scale().sm}px`,
                      opacity: props.audioMuted ? 0.5 : 1,
                    }}
                  >
                    {props.audioMuted ? "Muted" : "Playing"}
                  </button>
                  <button
                    onClick={props.onResetAudio}
                    title="Reset audio pipeline (Ctrl+Shift+A)"
                    style={{
                      ...ui.btn,
                      "font-size": `${scale().sm}px`,
                      opacity: 0.6,
                    }}
                  >
                    Reset
                  </button>
                </div>
              </Show>
            </div>
            <div
              style={{
                display: "flex",
                "flex-wrap": "wrap",
                gap: `${scale().tightGap}px`,
              }}
            >
              <For each={AUDIO_PRESETS}>
                {(preset) => (
                  <button
                    onClick={() => props.onAudioBitrateChange(preset.kbps)}
                    style={chipStyle(
                      props.audioBitrate === preset.kbps && !isCustom(),
                    )}
                  >
                    {preset.label}
                  </button>
                )}
              </For>
              <Show when={isCustom()}>
                <button style={chipStyle(true)}>
                  {props.audioBitrate} kbps
                </button>
              </Show>
            </div>
            <div
              style={{
                display: "flex",
                "align-items": "center",
                gap: `${scale().tightGap}px`,
              }}
            >
              <input
                type="text"
                inputmode="numeric"
                placeholder="Custom kbps"
                value={customBitrate()}
                onInput={(e) => setCustomBitrate(e.currentTarget.value)}
                onKeyDown={(e) => {
                  if (e.key === "Enter") handleCustomBitrate();
                  if (e.key === "Escape") props.onClose();
                }}
                style={{
                  ...ui.input,
                  flex: "0 1 120px",
                  "background-color": theme().inputBg,
                  color: "inherit",
                  "font-size": `${scale().sm}px`,
                  padding: `${scale().controlY}px ${scale().controlX}px`,
                }}
              />
              <button onClick={handleCustomBitrate} style={chipStyle(false)}>
                Set
              </button>
            </div>
          </div>

          {/* Video section */}
          <div style={sectionStyle()}>
            <span style={labelStyle()}>Video quality</span>
            <div
              style={{
                display: "flex",
                "flex-wrap": "wrap",
                gap: `${scale().tightGap}px`,
              }}
            >
              <For each={VIDEO_PRESETS}>
                {(preset) => (
                  <button
                    onClick={() => props.onVideoQualityChange(preset.value)}
                    style={chipStyle(props.videoQuality === preset.value)}
                  >
                    {preset.label}
                  </button>
                )}
              </For>
            </div>
          </div>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
