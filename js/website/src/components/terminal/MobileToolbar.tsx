import {
  createSignal,
  createEffect,
  onCleanup,
  Show,
  type JSX,
} from "solid-js";
import type { BlitTerminalSurface, SessionId } from "@blit-sh/core";
import { encoder } from "@blit-sh/core";
import type { BlitWorkspace } from "@blit-sh/core";

// ---------------------------------------------------------------------------
// Terminal byte sequences
// ---------------------------------------------------------------------------

const ARROW_UP = encoder.encode("\x1b[A");
const ARROW_DOWN = encoder.encode("\x1b[B");
const ARROW_RIGHT = encoder.encode("\x1b[C");
const ARROW_LEFT = encoder.encode("\x1b[D");
const ESC = encoder.encode("\x1b");
const TAB = encoder.encode("\t");

// ---------------------------------------------------------------------------
// Drag handle speed tiers
// ---------------------------------------------------------------------------

const DRAG_THRESHOLD = 10;
const DRAG_TIERS = [
  { distance: 60, interval: 50 },
  { distance: 30, interval: 100 },
  { distance: 0, interval: 200 },
];

function intervalForDelta(absDelta: number): number {
  for (const tier of DRAG_TIERS) {
    if (absDelta >= tier.distance) return tier.interval;
  }
  return 200;
}

// ---------------------------------------------------------------------------
// SVG icons (stroke-based)
// ---------------------------------------------------------------------------

const icon14 = {
  width: "14",
  height: "14",
  viewBox: "0 0 14 14",
  fill: "none",
  stroke: "currentColor",
  "stroke-width": "1.5",
  "stroke-linecap": "round" as const,
  "stroke-linejoin": "round" as const,
};

function ChevronUp() {
  return (
    <svg {...icon14}>
      <path d="M3 9 7 5l4 4" />
    </svg>
  );
}

function ChevronDown() {
  return (
    <svg {...icon14}>
      <path d="M3 5l4 4 4-4" />
    </svg>
  );
}

function ChevronLeftRight() {
  return (
    <svg {...icon14}>
      <path d="M5 3 1.5 7 5 11" />
      <path d="M9 3l3.5 4L9 11" />
    </svg>
  );
}

function TerminalIcon() {
  return (
    <svg {...icon14} viewBox="0 0 16 16">
      <path d="M3 5l4 4-4 4" />
      <path d="M9 13h4" />
    </svg>
  );
}

function CloseIcon() {
  return (
    <svg {...icon14}>
      <path d="M3 3l8 8M11 3l-8 8" />
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Arc layout: positions for 6 buttons around the FAB
// ---------------------------------------------------------------------------

const ARC_RADIUS = 72;
const ARC_ITEMS = 6;
// Arc spans 180° as a semicircle opening to the left of the FAB.
// 90° = down, 180° = left, 270° = up (in screen coordinates where Y+ is down).
const ARC_START = 90;
const ARC_END = 270;

function arcPosition(index: number): { x: number; y: number } {
  const angle = ARC_START + (index * (ARC_END - ARC_START)) / (ARC_ITEMS - 1);
  const rad = (angle * Math.PI) / 180;
  return {
    x: Math.cos(rad) * ARC_RADIUS,
    y: Math.sin(rad) * ARC_RADIUS,
  };
}

// ---------------------------------------------------------------------------
// ArcButton — a round button positioned in the arc
// ---------------------------------------------------------------------------

function ArcButton(props: {
  index: number;
  open: boolean;
  children: JSX.Element;
  onPress: () => void;
  active?: boolean;
  onPointerDown?: (e: PointerEvent) => void;
  onPointerMove?: (e: PointerEvent) => void;
  onPointerUp?: (e: PointerEvent) => void;
  onPointerCancel?: (e: PointerEvent) => void;
  onPointerLeave?: (e: PointerEvent) => void;
}) {
  const pos = arcPosition(props.index);

  return (
    <div
      class="absolute left-1/2 top-1/2 z-10"
      style={{
        transform: props.open
          ? `translate(calc(-50% + ${pos.x}px), calc(-50% + ${pos.y}px)) scale(1)`
          : "translate(-50%, -50%) scale(0)",
        opacity: props.open ? "1" : "0",
        "transition-property": "transform, opacity",
        "transition-duration": props.open ? "200ms" : "150ms",
        "transition-timing-function": "cubic-bezier(0.34, 1.56, 0.64, 1)",
        "transition-delay": props.open ? `${props.index * 30}ms` : "0ms",
        "pointer-events": props.open ? "auto" : "none",
      }}
    >
      <button
        type="button"
        onPointerDown={(e) => {
          e.preventDefault();
          e.stopPropagation();
          props.onPointerDown?.(e) ?? props.onPress();
        }}
        onPointerMove={props.onPointerMove}
        onPointerUp={props.onPointerUp}
        onPointerCancel={props.onPointerCancel}
        onPointerLeave={props.onPointerLeave}
        class={`flex items-center justify-center w-10 h-10 rounded-full border-2 shadow-md select-none active:scale-90 transition-transform ${
          props.active
            ? "bg-[var(--fg)] text-[var(--bg)] border-[var(--fg)]"
            : "bg-[var(--surface)] text-[var(--fg)] border-[var(--dim)]/30"
        }`}
      >
        {props.children}
      </button>
    </div>
  );
}

// ---------------------------------------------------------------------------
// SendArcButton — arc button that sends bytes on tap
// ---------------------------------------------------------------------------

function SendArcButton(props: {
  index: number;
  open: boolean;
  children: JSX.Element;
  bytes: Uint8Array;
  send: (bytes: Uint8Array) => void;
}) {
  return (
    <ArcButton
      index={props.index}
      open={props.open}
      onPress={() => props.send(props.bytes)}
    >
      {props.children}
    </ArcButton>
  );
}

// ---------------------------------------------------------------------------
// DragArcButton — arc button with horizontal drag for left/right arrows
// ---------------------------------------------------------------------------

function DragArcButton(props: {
  index: number;
  open: boolean;
  send: (bytes: Uint8Array) => void;
}) {
  let startX = 0;
  let active = false;
  let timer: ReturnType<typeof setInterval> | undefined;
  let currentDirection: Uint8Array | null = null;
  let currentInterval = 200;

  function stopRepeat() {
    clearInterval(timer);
    timer = undefined;
    currentDirection = null;
  }

  function updateRepeat(deltaX: number) {
    const absDelta = Math.abs(deltaX);
    if (absDelta < DRAG_THRESHOLD) {
      stopRepeat();
      return;
    }
    const direction = deltaX > 0 ? ARROW_RIGHT : ARROW_LEFT;
    const interval = intervalForDelta(absDelta);
    if (direction !== currentDirection || interval !== currentInterval) {
      stopRepeat();
      currentDirection = direction;
      currentInterval = interval;
      props.send(direction);
      timer = setInterval(() => props.send(direction), interval);
    }
  }

  onCleanup(() => stopRepeat());

  return (
    <ArcButton
      index={props.index}
      open={props.open}
      onPress={() => {}}
      onPointerDown={(e) => {
        e.preventDefault();
        e.stopPropagation();
        (e.target as HTMLElement).setPointerCapture(e.pointerId);
        startX = e.clientX;
        active = true;
      }}
      onPointerMove={(e) => {
        if (!active) return;
        updateRepeat(e.clientX - startX);
      }}
      onPointerUp={() => {
        active = false;
        stopRepeat();
      }}
      onPointerCancel={() => {
        active = false;
        stopRepeat();
      }}
      onPointerLeave={() => {}}
    >
      <ChevronLeftRight />
    </ArcButton>
  );
}

// ---------------------------------------------------------------------------
// MobileToolbar — FAB with radial arc
// ---------------------------------------------------------------------------

export default function MobileToolbar(props: {
  workspace: BlitWorkspace;
  focusedSessionId: () => SessionId | null;
  surface: () => BlitTerminalSurface | null;
  keyboardOpen: () => boolean;
}) {
  const [isOpen, setIsOpen] = createSignal(false);
  const [ctrlActive, setCtrlActive] = createSignal(false);

  // Ctrl sync
  let unsub: (() => void) | undefined;
  createEffect(() => {
    unsub?.();
    const surface = props.surface();
    if (surface) {
      unsub = surface.onCtrlModifierChange((active) => setCtrlActive(active));
    }
  });
  onCleanup(() => unsub?.());

  // Collapse when keyboard closes
  createEffect(() => {
    if (!props.keyboardOpen()) {
      setIsOpen(false);
    }
  });

  const send = (bytes: Uint8Array) => {
    const sid = props.focusedSessionId();
    if (sid) props.workspace.sendInput(sid, bytes);
  };

  const toggleCtrl = () => {
    const surface = props.surface();
    if (!surface) return;
    const next = !surface.ctrlModifier;
    surface.setCtrlModifier(next);
    setCtrlActive(next);
  };

  const visible = () => props.keyboardOpen();

  return (
    <Show when={visible()}>
      <div
        class="absolute z-20 right-3 top-1/2 -translate-y-1/2"
        style={{
          width: "48px",
          height: "48px",
          "pointer-events": "none",
          overflow: "visible",
        }}
      >
        {/* Backdrop to catch outside taps when expanded */}
        <Show when={isOpen()}>
          <div
            class="fixed inset-0 z-0"
            style={{ "pointer-events": "auto" }}
            onPointerDown={(e) => {
              e.preventDefault();
              setIsOpen(false);
            }}
          />
        </Show>

        {/* Arc buttons — origin at FAB center */}
        <div
          class="absolute"
          style={{
            left: "24px",
            top: "24px",
            width: "0",
            height: "0",
          }}
        >
          {/* Esc (bottom of arc) */}
          <ArcButton index={0} open={isOpen()} onPress={() => send(ESC)}>
            <span class="text-[10px] font-mono">Esc</span>
          </ArcButton>

          {/* Tab */}
          <ArcButton index={1} open={isOpen()} onPress={() => send(TAB)}>
            <span class="text-[10px] font-mono">Tab</span>
          </ArcButton>

          {/* Ctrl */}
          <ArcButton
            index={2}
            open={isOpen()}
            active={ctrlActive()}
            onPress={toggleCtrl}
          >
            <span class="text-[11px] font-mono font-medium">Ctrl</span>
          </ArcButton>

          {/* Left/Right drag handle */}
          <DragArcButton index={3} open={isOpen()} send={send} />

          {/* Arrow Down */}
          <SendArcButton
            index={4}
            open={isOpen()}
            bytes={ARROW_DOWN}
            send={send}
          >
            <ChevronDown />
          </SendArcButton>

          {/* Arrow Up (top of arc) */}
          <SendArcButton index={5} open={isOpen()} bytes={ARROW_UP} send={send}>
            <ChevronUp />
          </SendArcButton>
        </div>

        {/* FAB button */}
        <button
          type="button"
          onPointerDown={(e) => {
            e.preventDefault();
            setIsOpen((v) => !v);
          }}
          class="absolute inset-0 z-20 flex items-center justify-center rounded-full border-2 border-[var(--dim)]/40 bg-[var(--surface)] text-[var(--fg)] shadow-lg select-none active:scale-95 transition-transform"
          style={{
            "pointer-events": "auto",
          }}
        >
          <Show when={isOpen()} fallback={<TerminalIcon />}>
            <CloseIcon />
          </Show>
        </button>
      </div>
    </Show>
  );
}
