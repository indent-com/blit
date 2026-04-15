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
  { label: "Ultra", value: 4 },
];

/** Default slider positions when switching to custom for the first time. */
const CUSTOM_DEFAULT_QUANTIZER = 80;
const CUSTOM_DEFAULT_AUDIO_KBPS = 128;

export function MediaOverlay(props: {
  palette: TerminalPalette;
  fontSize: number;
  audioBitrate: number;
  videoQuality: number;
  audioMuted: boolean;
  audioAvailable: boolean;
  surfaceStreaming: boolean;
  onAudioBitrateChange: (kbps: number) => void;
  onVideoQualityChange: (quality: number) => void;
  onSurfaceStreamingChange: (enabled: boolean) => void;
  onToggleAudio: () => void;
  onResetAudio: () => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  // ---- Audio custom state ----
  const initCustomAudio =
    props.audioBitrate > 0 &&
    !AUDIO_PRESETS.some((p) => p.kbps === props.audioBitrate);

  const [customAudio, setCustomAudio] = createSignal(initCustomAudio);

  const [audioSlider, setAudioSlider] = createSignal(
    initCustomAudio ? props.audioBitrate : CUSTOM_DEFAULT_AUDIO_KBPS,
  );

  // ---- Video custom state ----
  const isCustomVideo = () => props.videoQuality >= 10;

  const [videoSlider, setVideoSlider] = createSignal(
    isCustomVideo() ? props.videoQuality : CUSTOM_DEFAULT_QUANTIZER,
  );

  // ---- Shared styles ----
  const cardStyle = (): JSX.CSSProperties => ({
    "background-color": theme().inputBg,
    border: `1px solid ${theme().subtleBorder}`,
    padding: `${scale().panelPadding}px`,
    display: "flex",
    "flex-direction": "column",
    gap: `${scale().gap}px`,
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

  const sliderRowStyle = (): JSX.CSSProperties => ({
    display: "flex",
    "align-items": "center",
    gap: `${scale().tightGap}px`,
  });

  const sliderLabelStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.5,
  });

  const sliderHintStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.6,
    "text-align": "center",
  });

  const sliderStyle = (): JSX.CSSProperties => ({
    flex: "1",
    "accent-color": theme().fg,
    cursor: "pointer",
  });

  // ---- Audio handlers ----
  const activateCustomAudio = () => {
    const k = customAudio() ? audioSlider() : CUSTOM_DEFAULT_AUDIO_KBPS;
    setCustomAudio(true);
    setAudioSlider(k);
    props.onAudioBitrateChange(k);
  };

  const handleAudioSlider = (e: Event) => {
    const v = parseInt((e.target as HTMLInputElement).value, 10);
    setAudioSlider(v);
    props.onAudioBitrateChange(v);
  };

  // ---- Video handlers ----
  const activateCustomVideo = () => {
    const q = isCustomVideo() ? videoSlider() : CUSTOM_DEFAULT_QUANTIZER;
    setVideoSlider(q);
    props.onVideoQualityChange(q);
  };

  const handleVideoSlider = (e: Event) => {
    const v = parseInt((e.target as HTMLInputElement).value, 10);
    setVideoSlider(v);
    props.onVideoQualityChange(v);
  };

  const quantizerHint = (): string => {
    const q = videoSlider();
    if (q <= 10) return "near-lossless";
    if (q <= 40) return "very high";
    if (q <= 80) return "high";
    if (q <= 120) return "medium";
    if (q <= 180) return "low";
    return "lowest";
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
          {/* ===== VIDEO CARD ===== */}
          <div style={cardStyle()}>
            <span style={labelStyle()}>Video</span>

            {/* Surface streaming toggle */}
            <div
              style={{
                display: "flex",
                "align-items": "center",
                "justify-content": "space-between",
              }}
            >
              <span style={{ "font-size": `${scale().md}px`, opacity: 0.8 }}>
                Surface streaming
              </span>
              <div style={{ display: "flex" }}>
                <button
                  onClick={() => props.onSurfaceStreamingChange(false)}
                  style={chipStyle(!props.surfaceStreaming)}
                >
                  Off
                </button>
                <button
                  onClick={() => props.onSurfaceStreamingChange(true)}
                  style={chipStyle(props.surfaceStreaming)}
                >
                  On
                </button>
              </div>
            </div>

            {/* Video quality — dimmed when streaming is off */}
            <div
              style={{
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().tightGap}px`,
                opacity: props.surfaceStreaming ? 1 : 0.35,
                "pointer-events": props.surfaceStreaming ? "auto" : "none",
                transition: "opacity 0.15s ease",
              }}
            >
              <span style={labelStyle()}>Quality</span>
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
                      style={chipStyle(
                        props.videoQuality === preset.value && !isCustomVideo(),
                      )}
                    >
                      {preset.label}
                    </button>
                  )}
                </For>
                <button
                  onClick={activateCustomVideo}
                  style={chipStyle(isCustomVideo())}
                >
                  Custom
                </button>
              </div>
              <Show when={isCustomVideo()}>
                <div
                  style={{
                    display: "flex",
                    "flex-direction": "column",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <div style={sliderRowStyle()}>
                    <span
                      style={{
                        ...sliderLabelStyle(),
                        "min-width": "3em",
                        "text-align": "right",
                      }}
                    >
                      Best
                    </span>
                    <input
                      type="range"
                      min="10"
                      max="255"
                      step="1"
                      value={videoSlider()}
                      onInput={handleVideoSlider}
                      style={sliderStyle()}
                    />
                    <span
                      style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}
                    >
                      Smallest
                    </span>
                  </div>
                  <span style={sliderHintStyle()}>
                    AV1 quantizer {videoSlider()} ({quantizerHint()})
                  </span>
                </div>
              </Show>
            </div>
          </div>

          {/* ===== AUDIO CARD ===== */}
          <div style={cardStyle()}>
            <span style={labelStyle()}>Audio</span>

            <Show when={props.audioAvailable}>
              {/* Audio playback toggle + reset */}
              <div
                style={{
                  display: "flex",
                  "align-items": "center",
                  "justify-content": "space-between",
                }}
              >
                <span style={{ "font-size": `${scale().md}px`, opacity: 0.8 }}>
                  Playback
                </span>
                <div
                  style={{
                    display: "flex",
                    "align-items": "center",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <div style={{ display: "flex" }}>
                    <button
                      onClick={() => {
                        if (!props.audioMuted) props.onToggleAudio();
                      }}
                      style={chipStyle(props.audioMuted)}
                    >
                      Off
                    </button>
                    <button
                      onClick={() => {
                        if (props.audioMuted) props.onToggleAudio();
                      }}
                      style={chipStyle(!props.audioMuted)}
                    >
                      On
                    </button>
                  </div>
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
              </div>
            </Show>

            {/* Bitrate — dimmed when audio is muted */}
            <div
              style={{
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().tightGap}px`,
                opacity: props.audioAvailable && !props.audioMuted ? 1 : 0.35,
                "pointer-events":
                  props.audioAvailable && !props.audioMuted ? "auto" : "none",
                transition: "opacity 0.15s ease",
              }}
            >
              <span style={labelStyle()}>Bitrate</span>
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
                      onClick={() => {
                        setCustomAudio(false);
                        props.onAudioBitrateChange(preset.kbps);
                      }}
                      style={chipStyle(
                        props.audioBitrate === preset.kbps && !customAudio(),
                      )}
                    >
                      {preset.label}
                    </button>
                  )}
                </For>
                <button
                  onClick={activateCustomAudio}
                  style={chipStyle(customAudio())}
                >
                  Custom
                </button>
              </div>
              <Show when={customAudio()}>
                <div
                  style={{
                    display: "flex",
                    "flex-direction": "column",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <div style={sliderRowStyle()}>
                    <span
                      style={{
                        ...sliderLabelStyle(),
                        "min-width": "2em",
                        "text-align": "right",
                      }}
                    >
                      8
                    </span>
                    <input
                      type="range"
                      min="8"
                      max="512"
                      step="8"
                      value={audioSlider()}
                      onInput={handleAudioSlider}
                      style={sliderStyle()}
                    />
                    <span
                      style={{ ...sliderLabelStyle(), "min-width": "2.5em" }}
                    >
                      512
                    </span>
                  </div>
                  <span style={sliderHintStyle()}>{audioSlider()} kbps</span>
                </div>
              </Show>
            </div>
          </div>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
