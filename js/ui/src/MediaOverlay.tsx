import { createSignal, Show, For, type JSX } from "solid-js";
import type { TerminalPalette } from "@blit-sh/core";
import { themeFor, ui, uiScale } from "./theme";
import {
  MAX_SURFACE_MAX_FPS,
  MAX_SURFACE_ZOOM,
  MIN_SURFACE_MAX_FPS,
  MIN_SURFACE_ZOOM,
  type SurfaceTouchMode,
  type SurfaceZoomMode,
} from "./storage";
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

const FPS_PRESETS: { label: string; value: number }[] = [
  { label: "Disabled", value: 0 },
  { label: "30 fps", value: 30 },
  { label: "60 fps", value: 60 },
  { label: "120 fps", value: 120 },
];

/** Zoom values are stored as integer percentages in both modes. */
const RELATIVE_ZOOM_PRESETS = [50, 75, 100, 125, 150, 200];
const EXACT_ZOOM_PRESETS = [50, 75, 100, 150, 200, 300, 400];

/** Default slider positions when switching to custom for the first time. */
const CUSTOM_DEFAULT_QUANTIZER = 80;
const CUSTOM_DEFAULT_SPEED = 128;
const CUSTOM_DEFAULT_AUDIO_KBPS = 128;
const CUSTOM_DEFAULT_FPS = 60;

export function MediaOverlay(props: {
  palette: TerminalPalette;
  fontSize: number;
  audioBitrate: number;
  videoBandwidth: number;
  videoSpeed: number;
  audioMuted: boolean;
  audioAvailable: boolean;
  surfaceStreaming: boolean;
  surfaceSmoothing: boolean;
  /** Per-surface source and delivery cadence ceiling. 0 means uncapped. */
  surfaceMaxFps: number;
  /** Surface zoom value in percent. */
  surfaceZoom: number;
  surfaceZoomMode: SurfaceZoomMode;
  surfaceTouchMode: SurfaceTouchMode;
  surfaceTouchAvailable: boolean;
  onAudioBitrateChange: (kbps: number) => void;
  onVideoBandwidthChange: (bandwidth: number) => void;
  onVideoSpeedChange: (speed: number) => void;
  onSurfaceStreamingChange: (enabled: boolean) => void;
  onSurfaceSmoothingChange: (enabled: boolean) => void;
  onSurfaceMaxFpsChange: (maxFps: number) => void;
  onSurfaceZoomChange: (percent: number) => void;
  onSurfaceZoomModeChange: (mode: SurfaceZoomMode) => void;
  onSurfaceTouchModeChange: (mode: SurfaceTouchMode) => void;
  onToggleAudio: () => void;
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

  // ---- Frame-rate custom state ----
  const isCustomFps = () =>
    props.surfaceMaxFps > 0 &&
    !FPS_PRESETS.some((preset) => preset.value === props.surfaceMaxFps);
  const [fpsSlider, setFpsSlider] = createSignal(
    isCustomFps() ? props.surfaceMaxFps : CUSTOM_DEFAULT_FPS,
  );

  // ---- Zoom custom state ----
  // Unlike the wire settings there is no reserved range here — any percent
  // off the preset list is a custom one, so the slider opens on it.
  const zoomPresets = (mode = props.surfaceZoomMode) =>
    mode === "exact" ? EXACT_ZOOM_PRESETS : RELATIVE_ZOOM_PRESETS;
  const initCustomZoom = !zoomPresets().includes(props.surfaceZoom);
  const [customZoom, setCustomZoom] = createSignal(initCustomZoom);
  const [zoomSlider, setZoomSlider] = createSignal(props.surfaceZoom);

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

  const chipStyle = (
    active: boolean,
    disabled = false,
  ): JSX.CSSProperties => ({
    ...ui.btn,
    padding: `${scale().controlY}px ${scale().controlX + 2}px`,
    border: `1px solid ${active ? theme().border : "transparent"}`,
    "background-color": active ? theme().selectedBg : "transparent",
    opacity: disabled ? 0.35 : active ? 1 : 0.7,
    "font-size": `${scale().sm}px`,
    cursor: disabled ? "not-allowed" : "pointer",
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

  const activateCustomFps = () => {
    const fps = isCustomFps() ? fpsSlider() : CUSTOM_DEFAULT_FPS;
    setFpsSlider(fps);
    props.onSurfaceMaxFpsChange(fps);
  };

  const handleFpsSlider = (e: Event) => {
    const fps = parseInt((e.target as HTMLInputElement).value, 10);
    setFpsSlider(fps);
    props.onSurfaceMaxFpsChange(fps);
  };

  /** The requested presentation scale after applying the selected mode. */
  const effectiveScale = (): number => {
    const dpr =
      typeof devicePixelRatio === "number" && devicePixelRatio > 0
        ? devicePixelRatio
        : 1;
    return (
      (props.surfaceZoomMode === "relative" ? dpr : 1) *
      (props.surfaceZoom / 100)
    );
  };

  const formatScale = (percent: number): string =>
    `${(percent / 100).toFixed(2).replace(/\.?0+$/, "")}×`;

  const formatZoom = (percent: number): string =>
    props.surfaceZoomMode === "exact" ? formatScale(percent) : `${percent}%`;

  const selectZoomMode = (mode: SurfaceZoomMode) => {
    setCustomZoom(!zoomPresets(mode).includes(props.surfaceZoom));
    setZoomSlider(props.surfaceZoom);
    props.onSurfaceZoomModeChange(mode);
  };

  const activateCustomZoom = () => {
    setCustomZoom(true);
    setZoomSlider(props.surfaceZoom);
  };

  const handleZoomSlider = (e: Event) => {
    const v = parseInt((e.target as HTMLInputElement).value, 10);
    setZoomSlider(v);
    props.onSurfaceZoomChange(v);
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

            <div
              style={{
                display: "flex",
                "align-items": "center",
                "justify-content": "space-between",
              }}
            >
              <span style={{ "font-size": `${scale().md}px`, opacity: 0.8 }}>
                Presentation
              </span>
              <div style={{ display: "flex" }}>
                <button
                  onClick={() => props.onSurfaceSmoothingChange(false)}
                  style={chipStyle(!props.surfaceSmoothing)}
                >
                  Low latency
                </button>
                <button
                  onClick={() => props.onSurfaceSmoothingChange(true)}
                  style={chipStyle(props.surfaceSmoothing)}
                >
                  Smooth
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
                <span style={labelStyle()}>Frame rate cap</span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <For each={FPS_PRESETS}>
                    {(preset) => (
                      <button
                        onClick={() =>
                          props.onSurfaceMaxFpsChange(preset.value)
                        }
                        style={chipStyle(
                          props.surfaceMaxFps === preset.value &&
                            !isCustomFps(),
                        )}
                      >
                        {preset.label}
                      </button>
                    )}
                  </For>
                  <button
                    onClick={activateCustomFps}
                    style={chipStyle(isCustomFps())}
                  >
                    Custom
                  </button>
                </div>
                <Show when={isCustomFps()}>
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
                        {MIN_SURFACE_MAX_FPS}
                      </span>
                      <input
                        type="range"
                        min={MIN_SURFACE_MAX_FPS}
                        max={MAX_SURFACE_MAX_FPS}
                        step="1"
                        value={fpsSlider()}
                        onInput={handleFpsSlider}
                        style={sliderStyle()}
                      />
                      <span
                        style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}
                      >
                        {MAX_SURFACE_MAX_FPS}
                      </span>
                    </div>
                  </div>
                </Show>
                <span style={sliderHintStyle()}>
                  {props.surfaceMaxFps > 0
                    ? `At most ${props.surfaceMaxFps} fps.`
                    : "Uses this display's refresh rate."}
                </span>
              </div>

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

              <div
                style={{
                  display: "flex",
                  "flex-direction": "column",
                  gap: `${scale().tightGap}px`,
                }}
              >
                <span style={labelStyle()}>Zoom</span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <button
                    onClick={() => selectZoomMode("relative")}
                    style={chipStyle(props.surfaceZoomMode === "relative")}
                  >
                    Relative to display
                  </button>
                  <button
                    onClick={() => selectZoomMode("exact")}
                    style={chipStyle(props.surfaceZoomMode === "exact")}
                  >
                    Exact scale
                  </button>
                </div>
                <span style={sliderHintStyle()}>
                  {props.surfaceZoomMode === "relative"
                    ? "A percentage of this display's DPI."
                    : "A fixed surface scale, independent of display DPI."}
                </span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <For each={zoomPresets()}>
                    {(preset) => (
                      <button
                        onClick={() => {
                          setCustomZoom(false);
                          props.onSurfaceZoomChange(preset);
                        }}
                        style={chipStyle(
                          props.surfaceZoom === preset && !customZoom(),
                        )}
                      >
                        {formatZoom(preset)}
                      </button>
                    )}
                  </For>
                  <button
                    onClick={activateCustomZoom}
                    style={chipStyle(customZoom())}
                  >
                    Custom
                  </button>
                </div>
                <Show when={customZoom()}>
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
                        {formatZoom(MIN_SURFACE_ZOOM)}
                      </span>
                      <input
                        type="range"
                        min={MIN_SURFACE_ZOOM}
                        max={MAX_SURFACE_ZOOM}
                        step="5"
                        value={zoomSlider()}
                        onInput={handleZoomSlider}
                        style={sliderStyle()}
                      />
                      <span
                        style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}
                      >
                        {formatZoom(MAX_SURFACE_ZOOM)}
                      </span>
                    </div>
                    <span style={sliderHintStyle()}>
                      {formatZoom(zoomSlider())}
                    </span>
                  </div>
                </Show>
                <span style={sliderHintStyle()}>
                  Requested surface scale:{" "}
                  {effectiveScale()
                    .toFixed(2)
                    .replace(/\.?0+$/, "")}
                  ×. Values below 1× zoom out and fit more in the pane.
                </span>
              </div>

              <div
                style={{
                  display: "flex",
                  "flex-direction": "column",
                  gap: `${scale().tightGap}px`,
                }}
              >
                <span style={labelStyle()}>Touch input</span>
                <div
                  style={{
                    display: "flex",
                    "flex-wrap": "wrap",
                    gap: `${scale().tightGap}px`,
                  }}
                >
                  <button
                    onClick={() => props.onSurfaceTouchModeChange("pointer")}
                    style={chipStyle(props.surfaceTouchMode === "pointer")}
                  >
                    Pointer gestures
                  </button>
                  <button
                    disabled={!props.surfaceTouchAvailable}
                    onClick={() => props.onSurfaceTouchModeChange("direct")}
                    style={chipStyle(
                      props.surfaceTouchMode === "direct",
                      !props.surfaceTouchAvailable,
                    )}
                  >
                    Direct multitouch
                  </button>
                </div>
                <span style={sliderHintStyle()}>
                  {props.surfaceTouchAvailable
                    ? props.surfaceTouchMode === "direct"
                      ? "Apps receive native contacts; tap, scroll, pinch, and drag behavior belongs to the app. Trackpads and pens are unchanged."
                      : "Touch keeps Blit's tap, scroll, long-press, and drag gestures."
                    : "Direct touch needs a server with multitouch support."}
                </span>
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
