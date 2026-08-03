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

const BANDWIDTH_PRESETS: { label: string; value: number }[] = [
  { label: "Default", value: 0 },
  { label: "Low", value: 1 },
  { label: "Medium", value: 2 },
  { label: "High", value: 3 },
  { label: "Ultra", value: 4 },
];

const SPEED_PRESETS: { label: string; value: number }[] = [
  { label: "Default", value: 0 },
  { label: "Slow", value: 1 },
  { label: "Medium", value: 2 },
  { label: "Fast", value: 3 },
  { label: "Realtime", value: 4 },
];

/** Default slider positions when switching to custom for the first time. */
const CUSTOM_DEFAULT_QUANTIZER = 80;
const CUSTOM_DEFAULT_SPEED = 128;
const CUSTOM_DEFAULT_AUDIO_KBPS = 128;

export function MediaOverlay(props: {
  palette: TerminalPalette;
  fontSize: number;
  audioBitrate: number;
  videoBandwidth: number;
  videoSpeed: number;
  audioMuted: boolean;
  audioAvailable: boolean;
  surfaceStreaming: boolean;
  onAudioBitrateChange: (kbps: number) => void;
  onVideoBandwidthChange: (bandwidth: number) => void;
  onVideoSpeedChange: (speed: number) => void;
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
  // Wire values 10–255 are the custom range on both axes; 0–4 are presets.
  const isCustomBandwidth = () => props.videoBandwidth >= 10;
  const isCustomSpeed = () => props.videoSpeed >= 10;

  const [bandwidthSlider, setBandwidthSlider] = createSignal(
    isCustomBandwidth() ? props.videoBandwidth : CUSTOM_DEFAULT_QUANTIZER,
  );

  const [speedSlider, setSpeedSlider] = createSignal(
    isCustomSpeed() ? props.videoSpeed : CUSTOM_DEFAULT_SPEED,
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
  const activateCustomBandwidth = () => {
    const q = isCustomBandwidth()
      ? bandwidthSlider()
      : CUSTOM_DEFAULT_QUANTIZER;
    setBandwidthSlider(q);
    props.onVideoBandwidthChange(q);
  };

  const handleBandwidthSlider = (e: Event) => {
    const v = parseInt((e.target as HTMLInputElement).value, 10);
    setBandwidthSlider(v);
    props.onVideoBandwidthChange(v);
  };

  const activateCustomSpeed = () => {
    const v = isCustomSpeed() ? speedSlider() : CUSTOM_DEFAULT_SPEED;
    setSpeedSlider(v);
    props.onVideoSpeedChange(v);
  };

  const handleSpeedSlider = (e: Event) => {
    const v = parseInt((e.target as HTMLInputElement).value, 10);
    setSpeedSlider(v);
    props.onVideoSpeedChange(v);
  };

  // The server treats the setting as a ceiling and spends less when the
  // link cannot carry it, so every label here reads as "at most".
  const bandwidthHint = (): string => {
    const v = bandwidthSlider();
    if (v <= 10) return "maximum";
    if (v <= 40) return "very high";
    if (v <= 80) return "high";
    if (v <= 120) return "medium";
    if (v <= 180) return "low";
    return "lowest";
  };

  // Thresholds line up with the presets: the server folds 10–255 onto a
  // 0–10 effort level, on which slow/medium/fast/realtime sit at 4/6/8/10.
  const speedHint = (): string => {
    const v = speedSlider();
    if (v <= 59) return "slowest";
    if (v <= 108) return "slow";
    if (v <= 157) return "medium";
    if (v <= 206) return "fast";
    return "fastest";
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

            {/* Bandwidth and speed — dimmed when streaming is off */}
            <div
              style={{
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().gap}px`,
                opacity: props.surfaceStreaming ? 1 : 0.35,
                "pointer-events": props.surfaceStreaming ? "auto" : "none",
                transition: "opacity 0.15s ease",
              }}
            >
              <div
                style={{
                  display: "flex",
                  "flex-direction": "column",
                  gap: `${scale().tightGap}px`,
                }}
              >
                <span style={labelStyle()}>Max bandwidth</span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <For each={BANDWIDTH_PRESETS}>
                    {(preset) => (
                      <button
                        onClick={() =>
                          props.onVideoBandwidthChange(preset.value)
                        }
                        style={chipStyle(
                          props.videoBandwidth === preset.value &&
                            !isCustomBandwidth(),
                        )}
                      >
                        {preset.label}
                      </button>
                    )}
                  </For>
                  <button
                    onClick={activateCustomBandwidth}
                    style={chipStyle(isCustomBandwidth())}
                  >
                    Custom
                  </button>
                </div>
                <Show when={isCustomBandwidth()}>
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
                        value={bandwidthSlider()}
                        onInput={handleBandwidthSlider}
                        style={sliderStyle()}
                      />
                      <span
                        style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}
                      >
                        Smallest
                      </span>
                    </div>
                    <span style={sliderHintStyle()}>
                      at most {bandwidthSlider()} ({bandwidthHint()})
                    </span>
                  </div>
                </Show>
              </div>

              <div
                style={{
                  display: "flex",
                  "flex-direction": "column",
                  gap: `${scale().tightGap}px`,
                }}
              >
                <span style={labelStyle()}>Speed</span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <For each={SPEED_PRESETS}>
                    {(preset) => (
                      <button
                        onClick={() => props.onVideoSpeedChange(preset.value)}
                        style={chipStyle(
                          props.videoSpeed === preset.value && !isCustomSpeed(),
                        )}
                      >
                        {preset.label}
                      </button>
                    )}
                  </For>
                  <button
                    onClick={activateCustomSpeed}
                    style={chipStyle(isCustomSpeed())}
                  >
                    Custom
                  </button>
                </div>
                <Show when={isCustomSpeed()}>
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
                        Slowest
                      </span>
                      <input
                        type="range"
                        min="10"
                        max="255"
                        step="1"
                        value={speedSlider()}
                        onInput={handleSpeedSlider}
                        style={sliderStyle()}
                      />
                      <span
                        style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}
                      >
                        Fastest
                      </span>
                    </div>
                    <span style={sliderHintStyle()}>
                      {speedSlider()} ({speedHint()})
                    </span>
                  </div>
                </Show>
              </div>
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
