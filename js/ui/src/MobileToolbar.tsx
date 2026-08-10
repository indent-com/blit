import { createSignal, createEffect, onCleanup } from "solid-js";
import type { BlitSurfaceCanvas, BlitTerminalSurface } from "@blit-sh/core";
import { surfaceCanvasForInput, terminalSurfaceForInput } from "@blit-sh/core";
import type { Theme, UIScale } from "./theme";

// ---------------------------------------------------------------------------
// Extra-key definitions
// ---------------------------------------------------------------------------

type ExtraKey = { key: string; code: string; shiftKey?: boolean };

const ESC: ExtraKey = { key: "Escape", code: "Escape" };
const TAB: ExtraKey = { key: "Tab", code: "Tab" };
const ARROW_UP: ExtraKey = { key: "ArrowUp", code: "ArrowUp" };
const ARROW_DOWN: ExtraKey = { key: "ArrowDown", code: "ArrowDown" };
const ARROW_RIGHT: ExtraKey = { key: "ArrowRight", code: "ArrowRight" };
const ARROW_LEFT: ExtraKey = { key: "ArrowLeft", code: "ArrowLeft" };

const CHAR_SLASH: ExtraKey = { key: "/", code: "Slash" };
const CHAR_PIPE: ExtraKey = {
  key: "|",
  code: "Backslash",
  shiftKey: true,
};
const CHAR_BACKSLASH: ExtraKey = { key: "\\", code: "Backslash" };
const CHAR_TILDE: ExtraKey = {
  key: "~",
  code: "Backquote",
  shiftKey: true,
};
const CHAR_BACKTICK: ExtraKey = { key: "`", code: "Backquote" };

type ModifierSurface = Pick<
  BlitTerminalSurface | BlitSurfaceCanvas,
  | "ctrlModifier"
  | "altModifier"
  | "setCtrlModifier"
  | "setAltModifier"
  | "onCtrlModifierChange"
  | "onAltModifierChange"
>;

function dispatchKey(target: HTMLElement, key: ExtraKey): void {
  const init: KeyboardEventInit = {
    key: key.key,
    code: key.code,
    shiftKey: key.shiftKey,
    bubbles: true,
    cancelable: true,
  };
  target.dispatchEvent(new KeyboardEvent("keydown", init));
  target.dispatchEvent(new KeyboardEvent("keyup", init));
}

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
  // When set, fire onPress from a real `click` instead of `pointerdown`.
  // iOS Safari only authorises clipboard reads inside a genuine click/touch
  // gesture, and preventDefault() on pointerdown suppresses that click.
  clickToActivate?: boolean;
  theme: Theme;
  scale: UIScale;
}) {
  return (
    <button
      type="button"
      disabled={props.disabled}
      onPointerDown={(e) => {
        // Click-activated buttons must let the native click through, so
        // don't preventDefault (which would cancel it on iOS Safari).
        if (!props.clickToActivate) e.preventDefault();
        e.stopPropagation();
        if (props.disabled || props.clickToActivate) return;
        props.onPress();
      }}
      on:touchstart={(e) => {
        // iPadOS blurs the focused terminal textarea when a tap lands on a
        // non-editable element, and cancelling pointerdown does not stop it —
        // the touch itself has to be cancelled or the software keyboard drops
        // on every toolbar tap.  Bound with on: so the listener sits on the
        // button: Solid's delegated onTouchStart is a document-level listener,
        // which Chromium makes passive, and a passive listener cannot
        // preventDefault.  Click-activated buttons (Paste) are exempt:
        // cancelling the touch would suppress the click their clipboard read
        // is authorised by.
        if (!props.clickToActivate) e.preventDefault();
      }}
      onClick={() => {
        if (!props.clickToActivate || props.disabled) return;
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
  key: ExtraKey;
  send: (key: ExtraKey) => void;
  theme: Theme;
  scale: UIScale;
}) {
  let timer: ReturnType<typeof setInterval> | undefined;
  let timeout: ReturnType<typeof setTimeout> | undefined;

  function start() {
    props.send(props.key);
    timeout = setTimeout(() => {
      timer = setInterval(() => props.send(props.key), 80);
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
      // See ToolbarButton: cancel the touch itself or iPadOS drops the
      // software keyboard on every toolbar tap.
      on:touchstart={(e) => e.preventDefault()}
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
  keyboardTarget: () => HTMLElement | null;
  theme: Theme;
  scale: UIScale;
}) {
  const [ctrlActive, setCtrlActive] = createSignal(false);
  const [altActive, setAltActive] = createSignal(false);
  const canPaste = typeof navigator !== "undefined" && !!navigator.clipboard;

  const modifierSurface = (): ModifierSurface | null => {
    const target = props.keyboardTarget();
    return terminalSurfaceForInput(target) ?? surfaceCanvasForInput(target);
  };

  // Sync Ctrl modifier state from surface
  let ctrlUnsub: (() => void) | undefined;
  createEffect(() => {
    ctrlUnsub?.();
    const surface = modifierSurface();
    if (surface) {
      setCtrlActive(surface.ctrlModifier);
      ctrlUnsub = surface.onCtrlModifierChange((active) =>
        setCtrlActive(active),
      );
    } else setCtrlActive(false);
  });
  onCleanup(() => ctrlUnsub?.());

  // Sync Alt modifier state from surface
  let altUnsub: (() => void) | undefined;
  createEffect(() => {
    altUnsub?.();
    const surface = modifierSurface();
    if (surface) {
      setAltActive(surface.altModifier);
      altUnsub = surface.onAltModifierChange((active) => setAltActive(active));
    } else setAltActive(false);
  });
  onCleanup(() => altUnsub?.());

  const send = (key: ExtraKey) => {
    const target = props.keyboardTarget();
    if (target) dispatchKey(target, key);
  };

  const handlePaste = () => {
    const target = props.keyboardTarget();
    if (!target) return;
    const terminal = terminalSurfaceForInput(target);
    if (terminal) {
      void terminal.pasteFromClipboard();
      // Keep the keyboard up: some browsers move focus to the tapped button.
      terminal.focus();
      return;
    }
    const surface = surfaceCanvasForInput(target);
    if (!surface) return;
    // Wayland paste is a native Ctrl+V chord. Dispatch it from this genuine
    // click so the surface's clipboard read retains transient activation.
    surface.setCtrlModifier(false);
    surface.setAltModifier(false);
    target.dispatchEvent(
      new KeyboardEvent("keydown", {
        key: "Control",
        code: "ControlLeft",
        ctrlKey: true,
        bubbles: true,
        cancelable: true,
      }),
    );
    for (const type of ["keydown", "keyup"] as const) {
      target.dispatchEvent(
        new KeyboardEvent(type, {
          key: "v",
          code: "KeyV",
          ctrlKey: true,
          bubbles: true,
          cancelable: true,
        }),
      );
    }
    target.dispatchEvent(
      new KeyboardEvent("keyup", {
        key: "Control",
        code: "ControlLeft",
        bubbles: true,
        cancelable: true,
      }),
    );
    target.focus();
  };

  const toggleCtrl = () => {
    const surface = modifierSurface();
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
    const surface = modifierSurface();
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
        // No safe-area inset here: the toolbar renders only while something
        // (software keyboard, iPadOS shortcut bar) is parked over the bottom
        // of the viewport, and that something covers the home-indicator strip.
        // The inset would sit between the keys and the keyboard as dead space;
        // the footer owns the inset again the moment the toolbar unmounts.
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
          clickToActivate
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
          key={ARROW_LEFT}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="→"
          title="Right"
          key={ARROW_RIGHT}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="↑"
          title="Up"
          key={ARROW_UP}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
        <ArrowButton
          label="↓"
          title="Down"
          key={ARROW_DOWN}
          send={send}
          theme={props.theme}
          scale={props.scale}
        />
      </div>
    </div>
  );
}
