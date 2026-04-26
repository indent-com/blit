import { createSignal, createEffect, onCleanup } from "solid-js";
import type { BlitTerminalSurface, SessionId } from "@blit-sh/core";
import { encoder } from "@blit-sh/core";
import type { BlitWorkspace } from "@blit-sh/core";
import type { Theme, UIScale } from "./theme";

// ---------------------------------------------------------------------------
// Terminal byte sequences
// ---------------------------------------------------------------------------

const ESC = encoder.encode("\x1b");
const TAB = encoder.encode("\t");
const ARROW_UP = encoder.encode("\x1b[A");
const ARROW_DOWN = encoder.encode("\x1b[B");
const ARROW_RIGHT = encoder.encode("\x1b[C");
const ARROW_LEFT = encoder.encode("\x1b[D");

const CHAR_SLASH = encoder.encode("/");
const CHAR_PIPE = encoder.encode("|");
const CHAR_BACKSLASH = encoder.encode("\\");
const CHAR_TILDE = encoder.encode("~");
const CHAR_BACKTICK = encoder.encode("`");

// ---------------------------------------------------------------------------
// ToolbarButton — a single button in the toolbar strip
// ---------------------------------------------------------------------------

function ToolbarButton(props: {
  label: string;
  title?: string;
  onPress: () => void;
  active?: boolean;
  wide?: boolean;
  disabled?: boolean;
  theme: Theme;
  scale: UIScale;
}) {
  return (
    <button
      type="button"
      disabled={props.disabled}
      onPointerDown={(e) => {
        e.preventDefault();
        e.stopPropagation();
        if (props.disabled) return;
        props.onPress();
      }}
      title={props.title}
      style={{
        background: props.active ? props.theme.fg : props.theme.inputBg,
        color: props.active ? props.theme.bg : props.theme.fg,
        border: `1px solid ${props.theme.subtleBorder}`,
        "border-radius": "4px",
        padding: `2px ${props.wide ? 10 : 6}px`,
        "min-width": "32px",
        height: "30px",
        "font-size": `${props.scale.sm}px`,
        "font-family": "ui-monospace, monospace",
        cursor: props.disabled ? "default" : "pointer",
        opacity: props.disabled ? 0.4 : 1,
        "flex-shrink": 0,
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        "user-select": "none",
        "-webkit-user-select": "none",
        "touch-action": "manipulation",
        "white-space": "nowrap",
        "line-height": 1,
        transition: "background 0.1s, color 0.1s, opacity 0.1s",
      }}
    >
      {props.label}
    </button>
  );
}

// ---------------------------------------------------------------------------
// ArrowButton — repeats on long-press
// ---------------------------------------------------------------------------

function ArrowButton(props: {
  label: string;
  title: string;
  bytes: Uint8Array;
  send: (bytes: Uint8Array) => void;
  theme: Theme;
  scale: UIScale;
}) {
  let timer: ReturnType<typeof setInterval> | undefined;
  let timeout: ReturnType<typeof setTimeout> | undefined;

  function start() {
    props.send(props.bytes);
    timeout = setTimeout(() => {
      timer = setInterval(() => props.send(props.bytes), 80);
    }, 300);
  }

  function stop() {
    clearTimeout(timeout);
    clearInterval(timer);
    timeout = undefined;
    timer = undefined;
  }

  onCleanup(stop);

  return (
    <button
      type="button"
      onPointerDown={(e) => {
        e.preventDefault();
        e.stopPropagation();
        start();
      }}
      onPointerUp={stop}
      onPointerCancel={stop}
      onPointerLeave={stop}
      title={props.title}
      style={{
        background: props.theme.inputBg,
        color: props.theme.fg,
        border: `1px solid ${props.theme.subtleBorder}`,
        "border-radius": "4px",
        padding: "2px 4px",
        "min-width": "32px",
        height: "30px",
        "font-size": `${props.scale.sm}px`,
        "font-family": "ui-monospace, monospace",
        cursor: "pointer",
        "flex-shrink": 0,
        display: "flex",
        "align-items": "center",
        "justify-content": "center",
        "user-select": "none",
        "-webkit-user-select": "none",
        "touch-action": "manipulation",
        "line-height": 1,
      }}
    >
      {props.label}
    </button>
  );
}

// ---------------------------------------------------------------------------
// MobileToolbar
// ---------------------------------------------------------------------------

export function MobileToolbar(props: {
  workspace: BlitWorkspace;
  focusedSessionId: () => SessionId | null;
  surface: () => BlitTerminalSurface | null;
  theme: Theme;
  scale: UIScale;
}) {
  const [ctrlActive, setCtrlActive] = createSignal(false);
  const [altActive, setAltActive] = createSignal(false);
  const canPaste = typeof navigator !== "undefined" && !!navigator.clipboard;

  // Sync Ctrl modifier state from surface
  let ctrlUnsub: (() => void) | undefined;
  createEffect(() => {
    ctrlUnsub?.();
    const surface = props.surface();
    if (surface) {
      ctrlUnsub = surface.onCtrlModifierChange((active) =>
        setCtrlActive(active),
      );
    }
  });
  onCleanup(() => ctrlUnsub?.());

  // Sync Alt modifier state from surface
  let altUnsub: (() => void) | undefined;
  createEffect(() => {
    altUnsub?.();
    const surface = props.surface();
    if (surface) {
      altUnsub = surface.onAltModifierChange((active) => setAltActive(active));
    }
  });
  onCleanup(() => altUnsub?.());

  const send = (bytes: Uint8Array) => {
    const sid = props.focusedSessionId();
    if (sid) props.workspace.sendInput(sid, bytes);
  };

  const handlePaste = () => {
    const surface = props.surface();
    if (!surface) return;
    void surface.pasteFromClipboard();
  };

  const toggleCtrl = () => {
    const surface = props.surface();
    if (!surface) return;
    const next = !surface.ctrlModifier;
    surface.setCtrlModifier(next);
    setCtrlActive(next);
    // If enabling ctrl, cancel alt
    if (next) {
      surface.setAltModifier(false);
      setAltActive(false);
    }
  };

  const toggleAlt = () => {
    const surface = props.surface();
    if (!surface) return;
    const next = !surface.altModifier;
    surface.setAltModifier(next);
    setAltActive(next);
    // If enabling alt, cancel ctrl
    if (next) {
      surface.setCtrlModifier(false);
      setCtrlActive(false);
    }
  };

  return (
    <div
      style={{
        display: "flex",
        "align-items": "center",
        "flex-wrap": "wrap-reverse",
        gap: "3px",
        padding: "4px 6px",
        "background-color": props.theme.bg,
        "border-top": `1px solid ${props.theme.subtleBorder}`,
        "flex-shrink": 0,
      }}
    >
      {/* Modifiers */}
      <div style={{ display: "flex", gap: "3px" }}>
        <ToolbarButton
          label="Esc"
          title="Escape"
          onPress={() => send(ESC)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="Tab"
          title="Tab"
          onPress={() => send(TAB)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="Ctrl"
          title="Ctrl modifier (one-shot)"
          onPress={toggleCtrl}
          active={ctrlActive()}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="Alt"
          title="Alt modifier (one-shot)"
          onPress={toggleAlt}
          active={altActive()}
          theme={props.theme}
          scale={props.scale}
        />
      </div>

      {/* Paste — Copy happens automatically on long-press selection */}
      <div style={{ display: "flex", gap: "3px" }}>
        <ToolbarButton
          label="Paste"
          title="Paste clipboard"
          onPress={handlePaste}
          disabled={!canPaste}
          wide
          theme={props.theme}
          scale={props.scale}
        />
      </div>

      {/* Character keys hard to reach on mobile keyboards */}
      <div style={{ display: "flex", gap: "3px" }}>
        <ToolbarButton
          label="/"
          onPress={() => send(CHAR_SLASH)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="|"
          onPress={() => send(CHAR_PIPE)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="\"
          onPress={() => send(CHAR_BACKSLASH)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="~"
          onPress={() => send(CHAR_TILDE)}
          theme={props.theme}
          scale={props.scale}
        />
        <ToolbarButton
          label="`"
          onPress={() => send(CHAR_BACKTICK)}
          theme={props.theme}
          scale={props.scale}
        />
      </div>

      {/* Arrow keys with repeat-on-hold */}
      <div style={{ display: "flex", gap: "3px" }}>
        <ArrowButton
          label="←"
          title="Left"
          bytes={ARROW_LEFT}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="→"
          title="Right"
          bytes={ARROW_RIGHT}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="↑"
          title="Up"
          bytes={ARROW_UP}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="↓"
          title="Down"
          bytes={ARROW_DOWN}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
      </div>
    </div>
  );
}
