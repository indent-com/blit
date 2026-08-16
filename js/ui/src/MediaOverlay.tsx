import {
  createEffect,
  createSignal,
  onCleanup,
  onMount,
  Show,
  For,
  type JSX,
} from "solid-js";
import {
  CODEC_SUPPORT_AV1,
  CODEC_SUPPORT_AV1_444,
  CODEC_SUPPORT_H264,
  CODEC_SUPPORT_H264_444,
  VIDEO_CODEC_AV1,
  VIDEO_CODEC_AV1_444,
  VIDEO_CODEC_H264,
  VIDEO_CODEC_H264_444,
  VIDEO_CODEC_MJPEG,
  type CameraQuality,
  type TerminalPalette,
} from "@blit-sh/core";
import { themeFor, ui, uiScale, type Theme, type UIScale } from "./theme";
import { t } from "./i18n";
import {
  CAMERA_FRAME_RATES,
  CAMERA_RESOLUTIONS,
  type MediaDevices,
} from "./mediaDevices";

import {
  MAX_SURFACE_ZOOM,
  MIN_SURFACE_MAX_FPS,
  MIN_SURFACE_ZOOM,
  type CameraChromaPreference,
  type CameraCodecPreference,
  type MicrophoneCodecPreference,
  type SurfaceTouchMode,
  type SurfaceZoomMode,
} from "./storage";
import { OverlayBackdrop, OverlayHeader, OverlayPanel } from "./Overlay";
import {
  CUSTOM_WIRE_MAX,
  CUSTOM_WIRE_MIN,
  detailWord,
  effortWord,
  flipWire,
  isCustomWire,
} from "./surfaceVideoPrefs";

/** Bitrate steps for the camera. The scale itself lives in `@blit-sh/core`,
 *  which is where the codec-specific currency (bitrate, JPEG quantizer) is. */
const CAMERA_QUALITIES: { label: string; value: CameraQuality }[] = [
  { label: "Less", value: "low" },
  { label: "Normal", value: "balanced" },
  { label: "More", value: "high" },
];

/** Camera upload codecs, against the `VIDEO_CODEC_*` bits a format needs. */
const CAMERA_CODECS: {
  label: string;
  value: CameraCodecPreference;
  bits: number;
}[] = [
  { label: "Auto", value: "auto", bits: 0 },
  { label: "Motion JPEG", value: "mjpeg", bits: VIDEO_CODEC_MJPEG },
  {
    label: "H.264",
    value: "h264",
    bits: VIDEO_CODEC_H264 | VIDEO_CODEC_H264_444,
  },
  { label: "AV1", value: "av1", bits: VIDEO_CODEC_AV1 | VIDEO_CODEC_AV1_444 },
];

const CAMERA_CHROMAS: { label: string; value: CameraChromaPreference }[] = [
  { label: "Auto", value: "auto" },
  { label: "Normal", value: "420" },
  { label: "Full colour", value: "444" },
];

const MICROPHONE_CODECS: { label: string; value: MicrophoneCodecPreference }[] =
  [
    { label: "Auto", value: "auto" },
    { label: "Opus", value: "opus" },
    { label: "Uncompressed", value: "pcm" },
  ];

const AUDIO_PRESETS: { label: string; kbps: number }[] = [
  { label: "Desktop default", kbps: 0 },
  { label: "32 kbps", kbps: 32 },
  { label: "64 kbps", kbps: 64 },
  { label: "96 kbps", kbps: 96 },
  { label: "128 kbps", kbps: 128 },
  { label: "192 kbps", kbps: 192 },
  { label: "256 kbps", kbps: 256 },
];

/**
 * Detail presets, low to high — the order the axis actually runs in.
 *
 * The wire byte is a quantizer ladder in disguise: 1–4 are the named presets
 * (Low = quantizer 180, Ultra = 1) and 10–255 is a raw AV1 quantizer where
 * *higher* is worse. So the presets climb while the custom range descends,
 * which is why the custom slider below is presented inverted.
 */
const DETAIL_PRESETS: { label: string; value: number }[] = [
  { label: "Desktop default", value: 0 },
  { label: "Low", value: 1 },
  { label: "Medium", value: 2 },
  { label: "High", value: 3 },
  { label: "Best", value: 4 },
];

/** Encoder effort, most to least. Wire 1 = Slow (most effort per bit) through
 *  4 = Realtime (the cheapest encode every backend offers). */
const EFFORT_PRESETS: { label: string; value: number }[] = [
  { label: "Desktop default", value: 0 },
  { label: "Most", value: 1 },
  { label: "More", value: 2 },
  { label: "Less", value: 3 },
  { label: "Least", value: 4 },
];

const FPS_PRESETS: { label: string; value: number }[] = [
  { label: "Display refresh", value: 0 },
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

/** Viewfinder cadence. A viewfinder needs neither the capture's frame rate nor
 *  its resolution, and this one runs on a tablet next to a live encoder. */
const PREVIEW_FPS = 15;

/**
 * Local camera viewfinder, copied frame by frame into a canvas.
 *
 * A `<video>` fed by a camera `MediaStream` does not paint on iPadOS 27.
 * Measured on the device: `readyState` 4, `currentTime` advancing, and live
 * pixels through `drawImage` — while the box on screen stays black, in every
 * setup ordering and with or without the styling below. Frame extraction reads
 * `videoFrameForCurrentTime()`, which is not the path the compositor uses, so
 * copying each frame into a canvas shows the picture the element will not draw
 * (WebKit bug 320979).
 *
 * One code path for every browser rather than a UA test: at this cadence and
 * size the copy costs nothing measurable, and the alternative is a viewfinder
 * whose one untestable branch is the one that was broken.
 */
function CameraPreview(props: { track: MediaStreamTrack; theme: Theme }) {
  let canvas!: HTMLCanvasElement;

  onMount(() => {
    // Its own element, not the encoder's: a camera track renders in as many
    // elements as ask (verified on device), and reaching into the capture to
    // share one buys nothing here.
    const video = document.createElement("video");
    video.muted = true;
    video.playsInline = true;
    video.srcObject = new MediaStream([props.track]);
    // Explicit, and after `srcObject`: `autoplay` alone is gated on the
    // element being inserted and on screen, which this one never is.
    void video.play().catch(() => {});

    const paint = () => {
      const width = canvas.clientWidth;
      const height = canvas.clientHeight;
      if (!width || !height) return;
      // Match the backing store to the box so the picture is not resampled
      // twice, but cap the ratio: a 3x tablet gains nothing at 8em tall.
      const ratio = Math.min(window.devicePixelRatio || 1, 2);
      const backingWidth = Math.round(width * ratio);
      const backingHeight = Math.round(height * ratio);
      if (canvas.width !== backingWidth) canvas.width = backingWidth;
      if (canvas.height !== backingHeight) canvas.height = backingHeight;
      const context = canvas.getContext("2d", { alpha: false });
      if (!context) return;
      context.fillStyle = props.theme.bg;
      context.fillRect(0, 0, backingWidth, backingHeight);
      const sourceWidth = video.videoWidth;
      const sourceHeight = video.videoHeight;
      if (video.readyState < 2 || !sourceWidth || !sourceHeight) return;
      // `contain`, computed here rather than left to CSS, because the source
      // is drawn rather than laid out.
      const scale = Math.min(
        backingWidth / sourceWidth,
        backingHeight / sourceHeight,
      );
      const drawWidth = sourceWidth * scale;
      const drawHeight = sourceHeight * scale;
      const left = (backingWidth - drawWidth) / 2;
      const top = (backingHeight - drawHeight) / 2;
      context.save();
      // Mirrored, the way a viewfinder is: this is the viewer checking
      // themselves, not the remote picture. Centred content stays centred
      // under the flip, so the offsets need no adjustment.
      context.translate(backingWidth, 0);
      context.scale(-1, 1);
      try {
        context.drawImage(video, left, top, drawWidth, drawHeight);
      } catch {
        // A track that ended between the readyState check and here throws
        // rather than drawing; the next tick finds `readyState` back at 0.
      }
      context.restore();
    };

    const timer = setInterval(paint, Math.round(1000 / PREVIEW_FPS));
    onCleanup(() => {
      clearInterval(timer);
      video.pause();
      video.srcObject = null;
    });
  });

  return (
    <canvas
      ref={canvas}
      aria-label={t("desktop.mediaCameraPreview")}
      role="img"
      style={{
        width: "100%",
        height: "8em",
        "background-color": props.theme.bg,
      }}
    />
  );
}

/** One device: which hardware to use, and whether it is currently shared. */
function DeviceRow(props: {
  label: string;
  devices: readonly MediaDeviceInfo[];
  selected: string;
  onSelect: (deviceId: string) => void;
  scale: UIScale;
  theme: Theme;
  sharing?: boolean;
  available?: boolean;
  busy?: boolean;
  onToggle?: () => void;
  unavailable?: string;
  /** Devices the browser listed but refused to name. */
  unnamed?: number;
}) {
  const chip = (active: boolean, disabled: boolean): JSX.CSSProperties => ({
    ...ui.btn,
    padding: `${props.scale.controlY}px ${props.scale.controlX + 2}px`,
    border: `1px solid ${active ? props.theme.border : "transparent"}`,
    "background-color": active ? props.theme.selectedBg : "transparent",
    "font-size": `${props.scale.sm}px`,
    opacity: disabled ? 0.35 : active ? 1 : 0.7,
    cursor: disabled ? "not-allowed" : "pointer",
  });
  const blocked = () =>
    Boolean(props.busy) || !(props.available || props.sharing);
  /** The stored id only if it still names a listed device.
   *
   *  A remembered device that has gone away — unplugged, or renamed by a
   *  browser that reissues its ids — must not be shown as the current choice:
   *  the share falls back to the default (the constraint is `ideal`), so
   *  claiming otherwise describes a camera nobody is using. */
  const effective = () =>
    props.devices.some((device) => device.deviceId === props.selected)
      ? props.selected
      : "";

  return (
    <div
      style={{
        display: "flex",
        "flex-direction": "column",
        gap: `${props.scale.tightGap}px`,
      }}
    >
      <div
        style={{
          display: "flex",
          "align-items": "center",
          "justify-content": "space-between",
          gap: `${props.scale.gap}px`,
        }}
      >
        <span style={{ "font-size": `${props.scale.md}px`, opacity: 0.8 }}>
          {props.label}
        </span>
        {/* Off/On rather than Share/Unshare: the label already says what the
            thing is, so the control only has to say which way it is set. */}
        <Show when={props.onToggle}>
          {(toggle) => (
            <div
              style={{ display: "flex" }}
              role="group"
              aria-label={props.label}
            >
              <button
                type="button"
                aria-pressed={!props.sharing}
                disabled={blocked()}
                onClick={() => props.sharing && toggle()()}
                style={chip(!props.sharing, blocked())}
              >
                Off
              </button>
              <button
                type="button"
                aria-pressed={Boolean(props.sharing)}
                disabled={blocked()}
                onClick={() => !props.sharing && toggle()()}
                style={chip(Boolean(props.sharing), blocked())}
              >
                On
              </button>
            </div>
          )}
        </Show>
      </div>
      <select
        // Re-applied whenever the option list is rebuilt, not just when the
        // choice changes: `enumerateDevices` hands back new objects every
        // call, so `For` discards every `option` and builds fresh ones — and
        // a `value` binding that only tracks the choice never re-runs, leaving
        // the select on its first option. That is a picker that forgets what
        // you told it every time a device is shared.
        ref={(element) => {
          createEffect(() => {
            // Read explicitly, not just through `effective()`: this effect is
            // only correct while it depends on the list, and a later short
            // circuit in there would silently stop tracking it.
            void props.devices;
            element.value = effective();
          });
        }}
        aria-label={props.label}
        onChange={(event) => props.onSelect(event.currentTarget.value)}
        style={{
          ...ui.btn,
          width: "100%",
          padding: `${props.scale.controlY}px ${props.scale.controlX}px`,
          "font-size": `${props.scale.sm}px`,
          "background-color": props.theme.inputBg,
          color: props.theme.fg,
          border: `1px solid ${props.theme.subtleBorder}`,
          opacity: 1,
        }}
      >
        <option value="">System default</option>
        <For each={props.devices}>
          {(device, index) => (
            <option value={device.deviceId}>
              {/* Labels stay blank until the page has been granted a device of
                  this kind, so an unnamed one is numbered rather than shown as
                  an empty row. Its id is real either way — that is what makes
                  it pickable. */}
              {device.label || `${props.label} ${index() + 1}`}
            </option>
          )}
        </For>
      </select>
      {/* Safari withholds ids as well as labels until a capture has been
          granted, and an option carrying `""` is the "System default" option
          under another name: picking it changes nothing, which reads as the
          picker snapping back. Say so instead of offering the choice. */}
      <Show when={props.devices.length === 0 && (props.unnamed ?? 0) > 0}>
        <span style={{ "font-size": `${props.scale.sm}px`, opacity: 0.6 }}>
          This browser hides which {props.label.toLowerCase()} is which until
          one has been shared. Turn it on once and the list fills in.
        </span>
      </Show>
      <Show when={props.unavailable && !props.available && !props.sharing}>
        <span
          style={{
            "font-size": `${props.scale.sm}px`,
            opacity: 0.6,
          }}
        >
          {props.unavailable}
        </span>
      </Show>
    </div>
  );
}

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
  waylandKeyboardRequests: boolean;
  /** Viewer camera/microphone/screen sharing. Created once at workspace
   *  scope — the panel drives it, but does not own it, because the status
   *  bar reads the same state for its privacy indicator. */
  devices: MediaDevices;
  /** Codecs accepted for surface video, as a CODEC_SUPPORT_* mask.
   *  0 means "no opinion": take whatever the decode probe found. */
  surfaceCodecs: number;
  /** What this browser's decode probe actually confirmed. */
  probedSurfaceCodecs: number;
  onSurfaceCodecsChange: (mask: number) => void;
  onAudioBitrateChange: (kbps: number) => void;
  onVideoBandwidthChange: (bandwidth: number) => void;
  onVideoSpeedChange: (speed: number) => void;
  onSurfaceStreamingChange: (enabled: boolean) => void;
  onSurfaceSmoothingChange: (enabled: boolean) => void;
  onSurfaceMaxFpsChange: (maxFps: number) => void;
  onSurfaceZoomChange: (percent: number) => void;
  onSurfaceZoomModeChange: (mode: SurfaceZoomMode) => void;
  onSurfaceTouchModeChange: (mode: SurfaceTouchMode) => void;
  onWaylandKeyboardRequestsChange: (enabled: boolean) => void;
  onToggleAudio: () => void;
  onClose: () => void;
}) {
  const theme = () => themeFor(props.palette);
  const scale = () => uiScale(props.fontSize);

  // ---- Disclosure state ----
  // Not persisted, matching the rest of the overlays: a settings panel that
  // remembers which drawer was open is a panel that opens differently every
  // time, and nothing here is expensive enough to be worth a storage key.
  const [openCapture, setOpenCapture] = createSignal(false);
  const [openPicture, setOpenPicture] = createSignal(false);
  const [openSound, setOpenSound] = createSignal(false);
  const [openInput, setOpenInput] = createSignal(false);
  const [openFormats, setOpenFormats] = createSignal(false);

  /**
   * Whether the format list is being chosen by hand.
   *
   * It cannot be derived from `props.surfaceCodecs`: `toggleReceive` stores 0
   * whenever the selection happens to equal the probe, so a deliberate choice
   * that matches what this browser can decode would snap the panel back to
   * Automatic under the user's cursor.
   */
  const [manualCodecs, setManualCodecs] = createSignal(
    props.surfaceCodecs !== 0,
  );

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
  const isCustomDetail = () => isCustomWire(props.videoBandwidth);
  const isCustomEffort = () => isCustomWire(props.videoSpeed);

  // Held flipped, the way it is displayed: the track runs low-to-high like the
  // preset row above it, while the byte it writes runs the other way.
  const [detailSlider, setDetailSlider] = createSignal(
    flipWire(
      isCustomDetail() ? props.videoBandwidth : CUSTOM_DEFAULT_QUANTIZER,
    ),
  );

  const [effortSlider, setEffortSlider] = createSignal(
    isCustomEffort() ? props.videoSpeed : CUSTOM_DEFAULT_SPEED,
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

  const sectionLabelStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.6,
    "text-transform": "uppercase",
    "letter-spacing": "0.05em",
  });

  const fieldLabelStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.75,
  });

  const chipStyle = (active: boolean, disabled = false): JSX.CSSProperties => ({
    ...ui.btn,
    padding: `${scale().controlY}px ${scale().controlX + 2}px`,
    border: `1px solid ${active ? theme().border : "transparent"}`,
    "background-color": active ? theme().selectedBg : "transparent",
    opacity: disabled ? 0.35 : active ? 1 : 0.7,
    "font-size": `${scale().sm}px`,
    cursor: disabled ? "not-allowed" : "pointer",
  });

  const rowStyle = (): JSX.CSSProperties => ({
    display: "flex",
    "flex-wrap": "wrap",
    gap: `${scale().tightGap}px`,
  });

  const stackStyle = (): JSX.CSSProperties => ({
    display: "flex",
    "flex-direction": "column",
    gap: `${scale().tightGap}px`,
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

  const hintStyle = (): JSX.CSSProperties => ({
    "font-size": `${scale().sm}px`,
    opacity: 0.6,
    "line-height": 1.35,
  });

  const sliderStyle = (): JSX.CSSProperties => ({
    flex: "1",
    "accent-color": theme().fg,
    cursor: "pointer",
  });

  // ---- Building blocks ----
  // Defined here rather than at module scope so they can read theme()/scale()
  // directly. A Solid component body runs once, so these are created once too.

  /** A titled card. */
  const Section = (p: { label: string; children: JSX.Element }) => (
    <section style={cardStyle()}>
      <span style={sectionLabelStyle()}>{p.label}</span>
      {p.children}
    </section>
  );

  /** A card whose heading *is* its disclosure, for sections that hold nothing
   *  but settings you touch once. A separate heading and toggle would say the
   *  same thing on two lines. */
  const CollapsibleSection = (p: {
    label: string;
    open: boolean;
    onToggle: () => void;
    children: JSX.Element;
  }) => (
    <section style={cardStyle()}>
      <button
        type="button"
        aria-expanded={p.open}
        onClick={p.onToggle}
        style={{
          ...ui.btn,
          display: "flex",
          "align-items": "center",
          gap: `${scale().tightGap}px`,
          padding: 0,
          border: "none",
          "background-color": "transparent",
          color: p.open ? theme().accent : "inherit",
          ...sectionLabelStyle(),
          opacity: p.open ? 0.9 : 0.6,
          "text-align": "left",
        }}
      >
        <span aria-hidden="true">{p.open ? "▾" : "▸"}</span>
        {p.label}
      </button>
      <Show when={p.open}>{p.children}</Show>
    </section>
  );

  /** A drawer for the settings that are read far less often than they are
   *  scrolled past. Real button, real aria-expanded — this is the panel's only
   *  navigation, so it has to be reachable from the keyboard. */
  const Disclosure = (p: {
    label: string;
    open: boolean;
    disabled?: boolean;
    onToggle: () => void;
    children: JSX.Element;
  }) => (
    <div style={stackStyle()}>
      <button
        type="button"
        aria-expanded={p.open}
        disabled={p.disabled}
        onClick={p.onToggle}
        style={{
          ...ui.btn,
          display: "flex",
          "align-items": "center",
          gap: `${scale().tightGap}px`,
          padding: `${scale().controlY}px 0`,
          border: "none",
          "background-color": "transparent",
          color: p.open ? theme().accent : "inherit",
          "font-size": `${scale().sm}px`,
          opacity: p.disabled ? 0.35 : p.open ? 1 : 0.7,
          cursor: p.disabled ? "not-allowed" : "pointer",
          "text-align": "left",
        }}
      >
        <span aria-hidden="true">{p.open ? "▾" : "▸"}</span>
        {p.label}
      </button>
      <Show when={p.open}>
        <div
          style={{
            ...stackStyle(),
            gap: `${scale().gap}px`,
            "padding-left": `${scale().controlX}px`,
            "border-left": `1px solid ${theme().subtleBorder}`,
          }}
        >
          {p.children}
        </div>
      </Show>
    </div>
  );

  /** A labelled setting: name, control, and — only when it earns the line —
   *  one sentence saying what the choice actually does. */
  const Field = (p: {
    label?: string;
    hint?: JSX.Element;
    children: JSX.Element;
  }) => (
    <div style={stackStyle()}>
      <Show when={p.label}>
        {(label) => <span style={fieldLabelStyle()}>{label()}</span>}
      </Show>
      {p.children}
      <Show when={p.hint}>
        {(hint) => <span style={hintStyle()}>{hint()}</span>}
      </Show>
    </div>
  );

  interface Choice {
    label: string;
    active: boolean;
    disabled?: boolean;
    /** Why this one cannot be picked. A greyed chip with no explanation is
     *  indistinguishable from a broken one. */
    title?: string;
    onSelect: () => void;
  }

  /** `disabled` here is the real attribute, never `pointer-events: none`: a
   *  dimmed-but-focusable control is still reachable by Tab and still fires on
   *  Enter, so the old wrapper let you change settings that looked dead. */
  const Chips = (p: {
    label: string;
    options: Choice[];
    disabled?: boolean;
  }) => (
    <div style={rowStyle()} role="group" aria-label={p.label}>
      <For each={p.options}>
        {(option) => {
          const off = () => p.disabled || option.disabled;
          return (
            <button
              type="button"
              aria-pressed={option.active}
              disabled={off()}
              title={option.title}
              onClick={option.onSelect}
              style={chipStyle(option.active, off())}
            >
              {option.label}
            </button>
          );
        }}
      </For>
    </div>
  );

  const Slider = (p: {
    label: string;
    min: number;
    max: number;
    step?: number;
    value: number;
    minLabel: string;
    maxLabel: string;
    readout: string;
    disabled?: boolean;
    onInput: (value: number) => void;
  }) => (
    <div style={stackStyle()}>
      <div style={sliderRowStyle()}>
        <span
          style={{
            ...sliderLabelStyle(),
            "min-width": "4em",
            "text-align": "right",
          }}
        >
          {p.minLabel}
        </span>
        <input
          type="range"
          aria-label={p.label}
          disabled={p.disabled}
          min={p.min}
          max={p.max}
          step={p.step ?? 1}
          value={p.value}
          onInput={(event) =>
            p.onInput(parseInt(event.currentTarget.value, 10))
          }
          style={sliderStyle()}
        />
        <span style={{ ...sliderLabelStyle(), "min-width": "4.5em" }}>
          {p.maxLabel}
        </span>
      </div>
      <span style={{ ...hintStyle(), "text-align": "center" }}>
        {p.readout}
      </span>
    </div>
  );

  /** Two-way switch. Distinct from Chips only in that it always has exactly
   *  two sides and sits on the same line as its label. */
  const Switch = (p: {
    label: string;
    off: string;
    on: string;
    value: boolean;
    disabled?: boolean;
    title?: string;
    onChange: (value: boolean) => void;
  }) => (
    <div
      style={{
        display: "flex",
        "align-items": "center",
        "justify-content": "space-between",
        gap: `${scale().gap}px`,
      }}
    >
      <span style={{ "font-size": `${scale().md}px`, opacity: 0.8 }}>
        {p.label}
      </span>
      <div style={{ display: "flex" }} role="group" aria-label={p.label}>
        <button
          type="button"
          aria-pressed={!p.value}
          disabled={p.disabled}
          title={p.title}
          onClick={() => p.onChange(false)}
          style={chipStyle(!p.value, p.disabled)}
        >
          {p.off}
        </button>
        <button
          type="button"
          aria-pressed={p.value}
          disabled={p.disabled}
          title={p.title}
          onClick={() => p.onChange(true)}
          style={chipStyle(p.value, p.disabled)}
        >
          {p.on}
        </button>
      </div>
    </div>
  );

  // ---- Audio handlers ----
  const activateCustomAudio = () => {
    const k = customAudio() ? audioSlider() : CUSTOM_DEFAULT_AUDIO_KBPS;
    setCustomAudio(true);
    setAudioSlider(k);
    props.onAudioBitrateChange(k);
  };

  // ---- Video handlers ----
  const activateCustomDetail = () => {
    const shown = isCustomDetail()
      ? detailSlider()
      : flipWire(CUSTOM_DEFAULT_QUANTIZER);
    setDetailSlider(shown);
    props.onVideoBandwidthChange(flipWire(shown));
  };

  const handleDetailSlider = (shown: number) => {
    setDetailSlider(shown);
    props.onVideoBandwidthChange(flipWire(shown));
  };

  const activateCustomEffort = () => {
    const v = isCustomEffort() ? effortSlider() : CUSTOM_DEFAULT_SPEED;
    setEffortSlider(v);
    props.onVideoSpeedChange(v);
  };

  const activateCustomFps = () => {
    const fps = isCustomFps() ? fpsSlider() : CUSTOM_DEFAULT_FPS;
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

  const trim = (value: number) => value.toFixed(2).replace(/\.?0+$/, "");

  const formatZoom = (percent: number): string =>
    props.surfaceZoomMode === "exact"
      ? `${trim(percent / 100)}×`
      : `${percent}%`;

  const selectZoomMode = (mode: SurfaceZoomMode) => {
    setCustomZoom(!zoomPresets(mode).includes(props.surfaceZoom));
    setZoomSlider(props.surfaceZoom);
    props.onSurfaceZoomModeChange(mode);
  };

  const activateCustomZoom = () => {
    setCustomZoom(true);
    setZoomSlider(props.surfaceZoom);
  };

  /** The desktop clamps a requested scale at 1× (compositor `scale_120.max(120)`),
   *  so an effective scale below that buys nothing at all. */
  const zoomClamped = () => effectiveScale() < 1;

  /** Nothing under App windows means anything while the windows are hidden. */
  const pictureOff = () => !props.surfaceStreaming;

  /** A 1–1000 track puts four frames a second under every pixel. Follow the
   *  stored value up when it is already past the useful end of the range,
   *  rather than pinning the thumb at a max below the live setting — which
   *  would rewrite it on the first drag. */
  const fpsSliderMax = () => Math.max(144, props.surfaceMaxFps);

  // ---- Devices ----
  const devices = () => props.devices;
  /** Connections that could take either device — the "send to" choice is
   *  one setting, not one per device kind. */
  const sharingTargets = () => {
    const seen = new Set<string>();
    return [
      ...devices().microphoneTargets(),
      ...devices().cameraTargets(),
    ].filter(
      (entry) => !seen.has(entry.snapshot.id) && seen.add(entry.snapshot.id),
    );
  };
  /** The remote a share would actually reach — the stored choice while it is
   *  still eligible, otherwise the one the fallback would pick. */
  const resolvedTarget = () =>
    devices().microphoneAvailable()?.snapshot.id ??
    devices().cameraAvailable()?.snapshot.id ??
    sharingTargets()[0]?.snapshot.id ??
    "";
  const selectStyle = (): JSX.CSSProperties => ({
    ...ui.btn,
    width: "100%",
    padding: `${scale().controlY}px ${scale().controlX}px`,
    "font-size": `${scale().sm}px`,
    "background-color": theme().inputBg,
    color: theme().fg,
    border: `1px solid ${theme().subtleBorder}`,
    opacity: 1,
  });

  /** Anything worth putting above the settings: a live share of ours, a cast
   *  the remote started, or a failure. All three are absent most of the time,
   *  and a status block that spends its life saying "nothing" is worse than
   *  no status block, so the whole section is conditional. */
  const liveMicrophone = () => devices().sharing("microphone");
  const liveCamera = () => devices().sharing("camera");
  const failure = () => devices().error() || devices().leaseError();
  const anythingLive = () =>
    Boolean(
      liveMicrophone() ||
      liveCamera() ||
      devices().activeScreenCasts().length > 0 ||
      failure(),
    );

  // ---- Receive codecs ----
  // The stored 0 means "no opinion", which is the probe's own answer. Seeding
  // the first edit from the probe rather than from 0xff keeps a toggle from
  // appearing to enable a codec this browser cannot decode.
  const receiveMask = () =>
    props.surfaceCodecs || props.probedSurfaceCodecs || 0xff;
  const receiveEnabled = (bit: number) =>
    Boolean(receiveMask() & bit) && Boolean(props.probedSurfaceCodecs & bit);
  /** A 4:4:4 entry offers a chroma of its base codec, which the server only
   *  ever reaches through that codec: `supports_444_by_client` is consulted
   *  after `supported_by_client` has already admitted the encoder. So it is
   *  not selectable on its own, and turning a base codec off takes it with. */
  const receiveSelectable = (bit: number, base?: number) => {
    if (!(props.probedSurfaceCodecs & bit)) return false;
    return base === undefined || Boolean(receiveMask() & base);
  };
  const receiveReason = (bit: number, base?: number, baseLabel?: string) => {
    if (!(props.probedSurfaceCodecs & bit)) {
      return "This browser cannot decode this format.";
    }
    if (base !== undefined && !(receiveMask() & base)) {
      return `Only offered through ${baseLabel}, which is off.`;
    }
    return undefined;
  };
  const toggleReceive = (bit: number, chroma = 0) => {
    const dropping = Boolean(receiveMask() & bit);
    const next = dropping
      ? receiveMask() & ~(bit | chroma)
      : receiveMask() | bit;
    // Refuse the toggle that would leave nothing decodable: an empty mask
    // reads as "accept anything" on the wire, so it would invert the setting.
    if (
      !(
        next &
        props.probedSurfaceCodecs &
        (CODEC_SUPPORT_H264 | CODEC_SUPPORT_AV1)
      )
    ) {
      return;
    }
    // Storing 0 when the selection matches the probe keeps a later browser
    // or GPU change flowing through instead of pinning today's answer.
    props.onSurfaceCodecsChange(next === props.probedSurfaceCodecs ? 0 : next);
  };

  /** One codec and its 4:4:4 variant, the dependency shown by nesting. */
  const CodecFamily = (p: {
    label: string;
    bit: number;
    chromaBit: number;
  }) => (
    <div style={rowStyle()}>
      <button
        type="button"
        aria-pressed={receiveEnabled(p.bit)}
        disabled={!receiveSelectable(p.bit)}
        title={receiveReason(p.bit)}
        onClick={() => toggleReceive(p.bit, p.chromaBit)}
        style={chipStyle(receiveEnabled(p.bit), !receiveSelectable(p.bit))}
      >
        {p.label}
      </button>
      <span
        aria-hidden="true"
        style={{ ...sliderLabelStyle(), "align-self": "center" }}
      >
        ↳
      </span>
      <button
        type="button"
        aria-pressed={receiveEnabled(p.chromaBit)}
        disabled={!receiveSelectable(p.chromaBit, p.bit)}
        title={receiveReason(p.chromaBit, p.bit, p.label)}
        onClick={() => toggleReceive(p.chromaBit)}
        style={chipStyle(
          receiveEnabled(p.chromaBit),
          !receiveSelectable(p.chromaBit, p.bit),
        )}
      >
        Full colour (4:4:4)
      </button>
    </div>
  );

  // ---- Send codecs ----
  const sendCodecs = () => devices().availableCameraCodecs();
  const chroma444Available = () => {
    const codec = devices().cameraCodec();
    const bits =
      codec === "h264"
        ? VIDEO_CODEC_H264_444
        : codec === "av1"
          ? VIDEO_CODEC_AV1_444
          : VIDEO_CODEC_H264_444 | VIDEO_CODEC_AV1_444;
    return Boolean(sendCodecs() & bits);
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
        style={{ "min-width": "320px", "max-width": "min(560px, 94vw)" }}
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
          {/* ===== LIVE NOW =====
              Only rendered when there is something to report. */}
          <Show when={anythingLive()}>
            <Section label="Live now">
              <Show when={liveMicrophone()}>
                {(entry) => (
                  <span style={{ "font-size": `${scale().sm}px` }}>
                    🎙 Microphone → {entry().label}
                  </span>
                )}
              </Show>
              <Show when={liveCamera()}>
                {(entry) => (
                  <span style={{ "font-size": `${scale().sm}px` }}>
                    📷 Camera → {entry().label}
                    <Show when={devices().cameraFormat()}>
                      {(format) => <> · {format()}</>}
                    </Show>
                  </span>
                )}
              </Show>

              {/* Screen sharing is started by an app through the portal, so
                  there is nothing to offer here but the stop button. */}
              <For each={devices().activeScreenCasts()}>
                {(entry) => (
                  <div
                    style={{
                      display: "flex",
                      "align-items": "center",
                      "justify-content": "space-between",
                      gap: `${scale().gap}px`,
                      "font-size": `${scale().sm}px`,
                    }}
                  >
                    <span>
                      🖥 {entry.session.appId || entry.label} ·{" "}
                      {t("desktop.mediaWindows")}{" "}
                      {entry.session.surfaceIds
                        .map((surfaceId) => `#${surfaceId}`)
                        .join(", ")}
                    </span>
                    <Show when={!entry.readOnly}>
                      <button
                        type="button"
                        onClick={() => devices().stopScreenCast(entry)}
                        style={chipStyle(true)}
                      >
                        {t("desktop.mediaStop")}
                      </button>
                    </Show>
                  </div>
                )}
              </For>
              <Show when={devices().activeScreenCasts().length > 0}>
                <span style={hintStyle()}>
                  An app on the desktop started this, not this browser.
                </span>
              </Show>

              {/* Both halves of "it did not work": the browser refusing the
                  device, and the server refusing the lease. The second does not
                  throw, so without it a refusal looks like a dead button. */}
              <Show when={failure()}>
                {(message) => (
                  <span
                    role="alert"
                    style={{
                      ...hintStyle(),
                      opacity: 1,
                      color: theme().errorText,
                    }}
                  >
                    {message()}
                  </span>
                )}
              </Show>
            </Section>
          </Show>

          {/* ===== SHARE FROM THIS DEVICE ===== */}
          <Section label="Share from this device">
            <DeviceRow
              label="Microphone"
              devices={devices().microphoneDevices()}
              unnamed={devices().unnamedMicrophones()}
              selected={devices().microphoneDevice()}
              onSelect={devices().setMicrophoneDevice}
              sharing={Boolean(liveMicrophone())}
              available={Boolean(devices().microphoneAvailable())}
              busy={devices().busy()}
              onToggle={() => devices().toggleShare("microphone")}
              unavailable="No connected desktop accepts microphone input."
              scale={scale()}
              theme={theme()}
            />

            <DeviceRow
              label="Camera"
              devices={devices().cameraDevices()}
              unnamed={devices().unnamedCameras()}
              selected={devices().cameraDevice()}
              onSelect={devices().setCameraDevice}
              sharing={Boolean(liveCamera())}
              available={Boolean(devices().cameraAvailable())}
              busy={devices().busy()}
              onToggle={() => devices().toggleShare("camera")}
              unavailable="No connected desktop accepts a camera format this browser can encode."
              scale={scale()}
              theme={theme()}
            />
            <Show when={devices().localCamera()}>
              {(entry) => (
                <Show when={entry().connection.mediaStore.cameraTrack}>
                  {(track) => <CameraPreview track={track()} theme={theme()} />}
                </Show>
              )}
            </Show>

            {/* Which remote a shared device goes to. Always shown, and always
                showing the one that would actually be used — a share that
                silently picks a desktop is not a share anyone consented to. */}
            <Field
              label="Send to"
              hint={
                sharingTargets().length > 1
                  ? "One choice for both devices; a desktop that cannot take one is skipped for it."
                  : undefined
              }
            >
              <Show
                when={sharingTargets().length > 0}
                fallback={
                  <span style={hintStyle()}>
                    No connected desktop accepts a camera or microphone.
                  </span>
                }
              >
                <select
                  value={resolvedTarget()}
                  aria-label="Send to"
                  onChange={(event) =>
                    devices().setTarget(event.currentTarget.value)
                  }
                  style={selectStyle()}
                >
                  <For each={sharingTargets()}>
                    {(entry) => (
                      <option value={entry.snapshot.id}>{entry.label}</option>
                    )}
                  </For>
                </select>
              </Show>
            </Field>

            {/* Everything about *how* the capture is encoded. Set once, if
                ever — and every one of these restarts a live share. */}
            <Disclosure
              label="Capture quality and formats"
              open={openCapture()}
              onToggle={() => setOpenCapture((v) => !v)}
            >
              {/* "Default", not "Auto": leaving it alone still sends a
                  request — 720p — rather than taking whatever turns up. */}
              <Field
                label="Camera resolution"
                hint="A request: the camera picks the nearest mode it has, this browser caps it at 1920×1080, and the desktop checks it again."
              >
                <Chips
                  label="Camera resolution"
                  options={[
                    {
                      label: "Default (720p)",
                      active: devices().cameraResolution() === 0,
                      onSelect: () => devices().setCameraResolution(0),
                    },
                    ...CAMERA_RESOLUTIONS.map((height) => ({
                      label: `${height}p`,
                      active: devices().cameraResolution() === height,
                      onSelect: () => devices().setCameraResolution(height),
                    })),
                  ]}
                />
              </Field>

              <Field
                label="Camera frame rate"
                hint="Also a request. Default asks for 30 fps, or 15 when Motion JPEG is all the two ends share."
              >
                <Chips
                  label="Camera frame rate"
                  options={[
                    {
                      label: "Default",
                      active: devices().cameraFrameRate() === 0,
                      onSelect: () => devices().setCameraFrameRate(0),
                    },
                    ...CAMERA_FRAME_RATES.map((fps) => ({
                      label: `${fps} fps`,
                      active: devices().cameraFrameRate() === fps,
                      onSelect: () => devices().setCameraFrameRate(fps),
                    })),
                  ]}
                />
              </Field>

              <Field
                label="Data spent on your camera picture"
                hint="Half, normal or double the bits for H.264 and AV1; JPEG quality 60%, 80% or 92% for Motion JPEG. This browser's own encoder applies it, so unlike the two requests above it always takes effect."
              >
                <Chips
                  label="Data spent on your camera picture"
                  options={CAMERA_QUALITIES.map((quality) => ({
                    label: quality.label,
                    active: devices().cameraQuality() === quality.value,
                    onSelect: () => devices().setCameraQuality(quality.value),
                  }))}
                />
              </Field>

              <Field
                label="Camera format"
                hint="Auto picks the best format this browser and the desktop both have."
              >
                <Chips
                  label="Camera format"
                  options={CAMERA_CODECS.map((codec) => {
                    const off =
                      codec.value !== "auto" && !(sendCodecs() & codec.bits);
                    return {
                      label: codec.label,
                      active: devices().cameraCodec() === codec.value,
                      disabled: off,
                      title: off
                        ? "Either this browser cannot encode it or no connected desktop accepts it."
                        : undefined,
                      onSelect: () => devices().setCameraCodec(codec.value),
                    };
                  })}
                />
              </Field>

              {/* Motion JPEG carries no chroma choice at all — asking for one
                  is an error rather than a hint — so the row goes away
                  entirely rather than sitting there greyed out. */}
              <Show when={devices().cameraCodec() !== "mjpeg"}>
                <Field
                  label="Camera colour detail"
                  hint="Full colour keeps text and thin lines sharp, for more bits."
                >
                  <Chips
                    label="Camera colour detail"
                    options={CAMERA_CHROMAS.map((chroma) => {
                      const off =
                        chroma.value === "444" && !chroma444Available();
                      return {
                        label: chroma.label,
                        active: devices().cameraChroma() === chroma.value,
                        disabled: off,
                        title: off
                          ? "No format this browser and desktop share offers 4:4:4."
                          : undefined,
                        onSelect: () => devices().setCameraChroma(chroma.value),
                      };
                    })}
                  />
                </Field>
              </Show>

              <Field
                label="Microphone format"
                hint="Opus costs far less bandwidth; uncompressed needs no encoder at all."
              >
                <Chips
                  label="Microphone format"
                  options={MICROPHONE_CODECS.map((codec) => {
                    const off =
                      codec.value === "opus" && !devices().opusAvailable();
                    return {
                      label: codec.label,
                      active: devices().microphoneCodec() === codec.value,
                      disabled: off,
                      title: off
                        ? "This browser has no Opus encoder."
                        : undefined,
                      onSelect: () => devices().setMicrophoneCodec(codec.value),
                    };
                  })}
                />
              </Field>

              <span style={hintStyle()}>
                A live share is stopped and restarted to pick any of these up,
                so the camera light blinks.
              </span>
            </Disclosure>
          </Section>

          {/* ===== APP WINDOWS ===== */}
          <Section label="App windows">
            <div
              style={{
                display: "flex",
                "flex-direction": "column",
                gap: `${scale().gap}px`,
              }}
            >
              <Field
                label="Size"
                hint={
                  <>
                    Higher gives the app a smaller window, so its own text comes
                    out bigger. Asking for {trim(effectiveScale())}× here.
                    {zoomClamped()
                      ? " The desktop clamps below 1×, so this is already as much as fits."
                      : ""}
                  </>
                }
              >
                <Chips
                  label="Size"
                  disabled={pictureOff()}
                  options={[
                    ...zoomPresets().map((preset) => ({
                      label: formatZoom(preset),
                      active: props.surfaceZoom === preset && !customZoom(),
                      onSelect: () => {
                        setCustomZoom(false);
                        props.onSurfaceZoomChange(preset);
                      },
                    })),
                    {
                      label: "Custom…",
                      active: customZoom(),
                      onSelect: activateCustomZoom,
                    },
                  ]}
                />
                <Show when={customZoom()}>
                  <Slider
                    label="Size"
                    disabled={pictureOff()}
                    min={MIN_SURFACE_ZOOM}
                    max={MAX_SURFACE_ZOOM}
                    step={5}
                    value={zoomSlider()}
                    minLabel={formatZoom(MIN_SURFACE_ZOOM)}
                    maxLabel={formatZoom(MAX_SURFACE_ZOOM)}
                    readout={formatZoom(zoomSlider())}
                    onInput={(value) => {
                      setZoomSlider(value);
                      props.onSurfaceZoomChange(value);
                    }}
                  />
                </Show>
              </Field>

              <Field
                label="Detail"
                hint="How much detail the desktop keeps. Not a byte cap: a busy screen still costs more, and a strained link looks worse than you asked."
              >
                <Chips
                  label="Detail"
                  disabled={pictureOff()}
                  options={[
                    ...DETAIL_PRESETS.map((preset) => ({
                      label: preset.label,
                      active:
                        props.videoBandwidth === preset.value &&
                        !isCustomDetail(),
                      onSelect: () =>
                        props.onVideoBandwidthChange(preset.value),
                    })),
                    {
                      label: "Custom…",
                      active: isCustomDetail(),
                      onSelect: activateCustomDetail,
                    },
                  ]}
                />
                <Show when={isCustomDetail()}>
                  <Slider
                    label="Detail"
                    disabled={pictureOff()}
                    min={CUSTOM_WIRE_MIN}
                    max={CUSTOM_WIRE_MAX}
                    value={detailSlider()}
                    minLabel="Lowest"
                    // Not "Highest": quantizer 10 is the top of the custom
                    // range and Best is quantizer 1, so this end genuinely
                    // cannot reach the chip above it. Saying so in the axis
                    // label costs nothing; saying it in a sentence costs a line.
                    maxLabel="Near best"
                    readout={`Detail: ${detailWord(flipWire(detailSlider()))}`}
                    onInput={handleDetailSlider}
                  />
                </Show>
              </Field>

              {/* Visible, not filed away: "the video stutters" and "everything
                  feels laggy" are the two complaints this one switch answers. */}
              <Field hint="Smoothest evens out playback by holding frames briefly; Lowest delay draws each one as it lands. Neither matters when the desktop is this machine.">
                <Switch
                  label="Motion"
                  off="Lowest delay"
                  on="Smoothest"
                  value={props.surfaceSmoothing}
                  disabled={pictureOff()}
                  onChange={props.onSurfaceSmoothingChange}
                />
              </Field>

              <Disclosure
                label="More picture settings"
                open={openPicture()}
                disabled={pictureOff()}
                onToggle={() => setOpenPicture((v) => !v)}
              >
                <Field
                  label="Frame rate limit"
                  hint={
                    props.surfaceMaxFps > 0
                      ? `A new frame at most ${props.surfaceMaxFps} times a second.`
                      : "New frames arrive as fast as this display refreshes."
                  }
                >
                  <Chips
                    label="Frame rate limit"
                    disabled={pictureOff()}
                    options={[
                      ...FPS_PRESETS.map((preset) => ({
                        label: preset.label,
                        active:
                          props.surfaceMaxFps === preset.value &&
                          !isCustomFps(),
                        onSelect: () =>
                          props.onSurfaceMaxFpsChange(preset.value),
                      })),
                      {
                        label: "Custom…",
                        active: isCustomFps(),
                        onSelect: activateCustomFps,
                      },
                    ]}
                  />
                  <Show when={isCustomFps()}>
                    <Slider
                      label="Frame rate limit"
                      disabled={pictureOff()}
                      min={MIN_SURFACE_MAX_FPS}
                      max={fpsSliderMax()}
                      value={fpsSlider()}
                      minLabel={`${MIN_SURFACE_MAX_FPS} fps`}
                      maxLabel={`${fpsSliderMax()} fps`}
                      readout={`${fpsSlider()} fps`}
                      onInput={(fps) => {
                        setFpsSlider(fps);
                        props.onSurfaceMaxFpsChange(fps);
                      }}
                    />
                  </Show>
                </Field>

                <Field
                  label="Compression effort"
                  hint="More effort means the same picture in fewer bits, paid for with CPU on the desktop."
                >
                  <Chips
                    label="Compression effort"
                    disabled={pictureOff()}
                    options={[
                      ...EFFORT_PRESETS.map((preset) => ({
                        label: preset.label,
                        active:
                          props.videoSpeed === preset.value &&
                          !isCustomEffort(),
                        onSelect: () => props.onVideoSpeedChange(preset.value),
                      })),
                      {
                        label: "Custom…",
                        active: isCustomEffort(),
                        onSelect: activateCustomEffort,
                      },
                    ]}
                  />
                  <Show when={isCustomEffort()}>
                    <Slider
                      label="Compression effort"
                      disabled={pictureOff()}
                      min={CUSTOM_WIRE_MIN}
                      max={CUSTOM_WIRE_MAX}
                      value={effortSlider()}
                      minLabel="Most"
                      maxLabel="Least"
                      readout={`Effort: ${effortWord(effortSlider())}`}
                      onInput={(value) => {
                        setEffortSlider(value);
                        props.onVideoSpeedChange(value);
                      }}
                    />
                  </Show>
                </Field>

                <Field
                  label="Size basis"
                  hint={
                    props.surfaceZoomMode === "relative"
                      ? "Size is a percentage of this display's pixel density, so text stays about the same physical size on a HiDPI screen."
                      : "Size is a fixed scale, ignoring this display's pixel density."
                  }
                >
                  <Chips
                    label="Size basis"
                    disabled={pictureOff()}
                    options={[
                      {
                        label: "Follow display density",
                        active: props.surfaceZoomMode === "relative",
                        onSelect: () => selectZoomMode("relative"),
                      },
                      {
                        label: "Fixed scale",
                        active: props.surfaceZoomMode === "exact",
                        onSelect: () => selectZoomMode("exact"),
                      },
                    ]}
                  />
                </Field>
              </Disclosure>
            </div>

            {/* Outside the dimmed block: it is the switch that dims it. */}
            <Field hint="Hidden stops their video and takes them out of this workspace. The apps keep running; terminals and sound are unaffected.">
              <Switch
                label="Show app windows"
                off="Hidden"
                on="Shown"
                value={props.surfaceStreaming}
                onChange={props.onSurfaceStreamingChange}
              />
            </Field>
          </Section>

          {/* ===== SOUND ===== */}
          <Section label="Sound">
            <Show
              when={props.audioAvailable}
              fallback={
                <span style={hintStyle()}>
                  No connected desktop offers sound.
                </span>
              }
            >
              <Switch
                label="Desktop sound"
                off="Off"
                on="On"
                value={!props.audioMuted}
                onChange={(on) => {
                  if (on === props.audioMuted) props.onToggleAudio();
                }}
              />

              {/* Playback is local — no lease, no server, nothing to share. */}
              <Show when={devices().speakerSelectionSupported}>
                <Field label="Play through">
                  <select
                    value={devices().speakerDevice()}
                    aria-label="Play through"
                    onChange={(event) =>
                      devices().setSpeakerDevice(event.currentTarget.value)
                    }
                    style={selectStyle()}
                  >
                    <option value="">System default</option>
                    <For each={devices().speakerDevices()}>
                      {(device, index) => (
                        <option value={device.deviceId}>
                          {device.label || `Output ${index() + 1}`}
                        </option>
                      )}
                    </For>
                  </select>
                </Field>
              </Show>

              <Disclosure
                label="Sound quality"
                open={openSound()}
                onToggle={() => setOpenSound((v) => !v)}
              >
                {/* One Opus encoder serves every subscribed viewer and the
                    server takes the max of what they ask for, so this is not
                    a private setting. */}
                <Field
                  label="Bitrate"
                  hint="Sound from the desktop, not your microphone. Every viewer shares one encoder, so the highest figure anyone asks for is what all of them get."
                >
                  <div style={stackStyle()}>
                    <Chips
                      label="Bitrate"
                      disabled={props.audioMuted}
                      options={[
                        ...AUDIO_PRESETS.map((preset) => ({
                          label: preset.label,
                          active:
                            props.audioBitrate === preset.kbps &&
                            !customAudio(),
                          onSelect: () => {
                            setCustomAudio(false);
                            props.onAudioBitrateChange(preset.kbps);
                          },
                        })),
                        {
                          label: "Custom…",
                          active: customAudio(),
                          onSelect: activateCustomAudio,
                        },
                      ]}
                    />
                    <Show when={customAudio()}>
                      <Slider
                        label="Bitrate"
                        disabled={props.audioMuted}
                        min={8}
                        max={512}
                        step={8}
                        value={audioSlider()}
                        minLabel="8 kbps"
                        maxLabel="512 kbps"
                        readout={`${audioSlider()} kbps`}
                        onInput={(kbps) => {
                          setAudioSlider(kbps);
                          props.onAudioBitrateChange(kbps);
                        }}
                      />
                    </Show>
                  </div>
                </Field>
              </Disclosure>
            </Show>
          </Section>

          {/* ===== TOUCH AND KEYBOARD =====
              Not media, but this is the only panel that configures how a
              remote app is driven, and a settings drawer of its own for two
              controls would be worse than a clearly-named section here. */}
          <CollapsibleSection
            label="Touch and keyboard"
            open={openInput()}
            onToggle={() => setOpenInput((v) => !v)}
          >
            <>
              <Field
                label="Touch"
                hint={
                  props.surfaceTouchAvailable
                    ? props.surfaceTouchMode === "direct"
                      ? "Apps receive native contacts, so tap, scroll, pinch and drag behave however the app wants. Trackpads and pens are unchanged."
                      : "Touch keeps Blit's own tap, scroll, long-press and drag gestures."
                    : "Sending touches needs a desktop with multitouch support."
                }
              >
                <Chips
                  label="Touch"
                  options={[
                    {
                      label: "Blit gestures",
                      active: props.surfaceTouchMode === "pointer",
                      onSelect: () => props.onSurfaceTouchModeChange("pointer"),
                    },
                    {
                      label: "Send touches to the app",
                      active: props.surfaceTouchMode === "direct",
                      disabled: !props.surfaceTouchAvailable,
                      title: props.surfaceTouchAvailable
                        ? undefined
                        : "No connected desktop supports multitouch.",
                      onSelect: () => props.onSurfaceTouchModeChange("direct"),
                    },
                  ]}
                />
              </Field>

              <Field
                label="On-screen keyboard"
                hint={
                  props.waylandKeyboardRequests
                    ? "A focused app can raise and lower this device's keyboard."
                    : "Apps cannot open the keyboard; only the status-bar keyboard control does."
                }
              >
                <Chips
                  label="On-screen keyboard"
                  options={[
                    {
                      label: "Apps may open it",
                      active: props.waylandKeyboardRequests,
                      onSelect: () =>
                        props.onWaylandKeyboardRequestsChange(true),
                    },
                    {
                      label: "Only when I ask",
                      active: !props.waylandKeyboardRequests,
                      onSelect: () =>
                        props.onWaylandKeyboardRequestsChange(false),
                    },
                  ]}
                />
              </Field>
            </>
          </CollapsibleSection>

          {/* ===== FORMATS THIS BROWSER ACCEPTS ===== */}
          <CollapsibleSection
            label="Video formats this browser accepts"
            open={openFormats()}
            onToggle={() => setOpenFormats((v) => !v)}
          >
            <>
              <Show
                when={props.probedSurfaceCodecs}
                fallback={
                  <span style={hintStyle()}>
                    Still working out what this browser can decode…
                  </span>
                }
              >
                {/* Automatic and the per-format list are a mode, not peers.
                    Shown side by side they both read as active — the list is
                    seeded from the probe, so every decodable format lights up
                    under Automatic too. */}
                <Field
                  hint={
                    manualCodecs()
                      ? "The desktop encodes with the best of these it has. Greyed-out formats are ones this browser cannot decode."
                      : "Follows what this browser proves it can decode, and keeps following it after a browser or driver update."
                  }
                >
                  <Chips
                    label="How formats are chosen"
                    options={[
                      {
                        label: "Automatic",
                        active: !manualCodecs(),
                        onSelect: () => {
                          setManualCodecs(false);
                          props.onSurfaceCodecsChange(0);
                        },
                      },
                      {
                        label: "Choose…",
                        active: manualCodecs(),
                        onSelect: () => setManualCodecs(true),
                      },
                    ]}
                  />
                </Field>
                <Show when={manualCodecs()}>
                  <div style={stackStyle()}>
                    <CodecFamily
                      label="H.264"
                      bit={CODEC_SUPPORT_H264}
                      chromaBit={CODEC_SUPPORT_H264_444}
                    />
                    <CodecFamily
                      label="AV1"
                      bit={CODEC_SUPPORT_AV1}
                      chromaBit={CODEC_SUPPORT_AV1_444}
                    />
                    <span style={hintStyle()}>
                      A desktop only ever offers full colour through the format
                      to its left, so turning that off turns it off too. The
                      last decodable format cannot be turned off — an empty list
                      would read as "send anything".
                    </span>
                  </div>
                </Show>
              </Show>
            </>
          </CollapsibleSection>
        </div>
      </OverlayPanel>
    </OverlayBackdrop>
  );
}
