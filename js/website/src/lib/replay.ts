/**
 * Replay a recorded blit session through the real client.
 *
 * The hero on blit.sh does not fake its terminal: the frames it plays were
 * recorded from a live `blit server` with `blit terminal record`, and they
 * travel the production pipeline — BlitConnection, the WASM diff engine,
 * the WebGPU/WebGL/canvas2d renderer. What replaces the network is this
 * transport, which performs the server's side of the handshake from
 * constants and then emits the recorded `S2C_UPDATE` bytes on the recorded
 * schedule. Client sends are swallowed; there is no server to hear them.
 *
 * Loops are free because the first frame after a subscribe is a full grid
 * snapshot: feeding it again simply stomps the screen back to the start.
 */

import {
  S2C_HELLO,
  S2C_LIST,
  S2C_READY,
  S2C_UPDATE,
  type BlitTransport,
  type BlitTransportEventMap,
  type ConnectionStatus,
} from "@blit-sh/core/types";

export interface ReplayFrame {
  /** Microseconds since the recording started. */
  t: number;
  data: Uint8Array;
}

/** One recorded pty: `blit terminal record` output plus its identity. */
export interface ReplayStream {
  ptyId: number;
  tag: string;
  frames: ReplayFrame[];
}

/** Parse a `.blitrec` file (magic, then [t_us:8][len:4][bytes] records). */
export function parseBlitrec(buf: ArrayBuffer): ReplayFrame[] {
  const bytes = new Uint8Array(buf);
  const view = new DataView(buf);
  const magic = new TextDecoder().decode(bytes.subarray(0, 8));
  if (magic !== "BLITREC\n") throw new Error("not a BLITREC file");
  const frames: ReplayFrame[] = [];
  let off = 8;
  while (off + 12 <= bytes.length) {
    const t = Number(view.getBigUint64(off, true));
    const len = view.getUint32(off + 8, true);
    frames.push({ t, data: bytes.subarray(off + 12, off + 12 + len) });
    off += 12 + len;
  }
  return frames;
}

const enc = new TextEncoder();

function helloMsg(): Uint8Array {
  // [op][version:2][features:4] — the short pre-boot-generation form the
  // client still accepts. No features: the hero needs no fs/git/lsp.
  const msg = new Uint8Array(7);
  const v = new DataView(msg.buffer);
  msg[0] = S2C_HELLO;
  v.setUint16(1, 1, true);
  v.setUint32(3, 0, true);
  return msg;
}

function listMsg(streams: ReplayStream[]): Uint8Array {
  const parts: number[] = [
    S2C_LIST,
    streams.length & 0xff,
    (streams.length >> 8) & 0xff,
  ];
  for (const { ptyId, tag } of streams) {
    const tagBytes = enc.encode(tag);
    parts.push(ptyId & 0xff, (ptyId >> 8) & 0xff);
    parts.push(tagBytes.length & 0xff, (tagBytes.length >> 8) & 0xff);
    for (const b of tagBytes) parts.push(b);
    parts.push(0, 0); // empty command
  }
  return new Uint8Array(parts);
}

function updateMsg(ptyId: number, payload: Uint8Array): Uint8Array {
  const msg = new Uint8Array(3 + payload.length);
  msg[0] = S2C_UPDATE;
  msg[1] = ptyId & 0xff;
  msg[2] = (ptyId >> 8) & 0xff;
  msg.set(payload, 3);
  return msg;
}

export interface ReplayOptions {
  /** Pause this long at the end before looping (ms). */
  holdMs?: number;
  /** Jump straight to the settled end state and stay there —
   *  `prefers-reduced-motion`'s reading of a looping demo. */
  static?: boolean;
}

export class ReplayTransport implements BlitTransport {
  private _status: ConnectionStatus = "connecting";
  private messageListeners = new Set<(data: ArrayBuffer) => void>();
  private statusListeners = new Set<(status: ConnectionStatus) => void>();
  authRejected = false;
  lastError: string | null = null;

  /** Every frame across streams, merged and sorted by recorded time. */
  private timeline: { t: number; msg: Uint8Array }[] = [];
  private timer: ReturnType<typeof setTimeout> | null = null;
  private cursor = 0;
  private startedAt = 0;
  private playing = false;
  // True from the last frame of a pass until the loop restarts: startedAt
  // sits in the future during the hold, so elapsed time goes negative and
  // would read as "before the story began" — exactly wrong for chrome that
  // should stay settled while the finished session is on screen.
  private holding = false;

  constructor(
    private streams: ReplayStream[],
    private opts: ReplayOptions = {},
  ) {
    for (const s of streams) {
      for (const f of s.frames) {
        this.timeline.push({ t: f.t / 1000, msg: updateMsg(s.ptyId, f.data) });
      }
    }
    this.timeline.sort((a, b) => a.t - b.t);
  }

  get status() {
    return this._status;
  }

  connect() {
    this.setStatus("connected");
    this.emit(helloMsg());
    this.emit(listMsg(this.streams));
    this.emit(new Uint8Array([S2C_READY]));
    if (this.opts.static) {
      for (const f of this.timeline) this.emit(f.msg);
      return;
    }
    this.play();
  }

  /** Resume the schedule (no-op while already playing, or in static mode). */
  play() {
    if (this.opts.static) return;
    if (this.playing || this.timeline.length === 0) return;
    this.playing = true;
    this.startedAt = performance.now() - (this.timeline[this.cursor]?.t ?? 0);
    this.tick();
  }

  /** Milliseconds into the story, for choreographing chrome around the
   *  replay (a file's status flipping when the recorded fix lands). Holds
   *  at the end during the loop pause, and rewinds with the loop. */
  position(): number {
    if (this.timeline.length === 0) return 0;
    const end = this.timeline[this.timeline.length - 1].t;
    // During the end-of-loop hold the clock reads "the end", not zero:
    // `startedAt` sits in the future then, so elapsed time goes negative,
    // and chrome keyed to it would snap back to the story's start while
    // the finished session is still on screen.
    if (this.opts.static || this.holding) return end;
    if (!this.playing) {
      return this.cursor > 0
        ? Math.min(this.timeline[this.cursor - 1].t, end)
        : 0;
    }
    return Math.max(0, Math.min(performance.now() - this.startedAt, end));
  }

  /** Stop emitting frames; the screen keeps its last state. */
  pause() {
    this.playing = false;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
  }

  private tick = () => {
    this.timer = null;
    if (!this.playing) return;
    const now = performance.now() - this.startedAt;
    // The restart tick after a hold: frames are about to flow again, so
    // the clock leaves "the end" and rewinds with the loop.
    if (now >= 0) this.holding = false;
    // Emit everything due; when the tab was hidden this fast-forwards
    // rather than queueing a burst of stale timers.
    while (
      this.cursor < this.timeline.length &&
      this.timeline[this.cursor].t <= now
    ) {
      this.emit(this.timeline[this.cursor].msg);
      this.cursor++;
    }
    if (this.cursor >= this.timeline.length) {
      this.cursor = 0;
      this.holding = true;
      this.startedAt = performance.now() + (this.opts.holdMs ?? 4000);
      this.timer = setTimeout(this.tick, this.opts.holdMs ?? 4000);
      return;
    }
    const delay = Math.max(0, this.timeline[this.cursor].t - now);
    this.timer = setTimeout(this.tick, delay);
  };

  send(_data: Uint8Array) {
    // A replay has no server: input, resizes and acks fall on the floor.
  }

  close() {
    this.pause();
    this.setStatus("closed");
  }

  addEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.add(listener as (data: ArrayBuffer) => void);
    } else {
      this.statusListeners.add(listener as (s: ConnectionStatus) => void);
    }
  }

  removeEventListener<K extends keyof BlitTransportEventMap>(
    type: K,
    listener: (data: BlitTransportEventMap[K]) => void,
  ): void {
    if (type === "message") {
      this.messageListeners.delete(listener as (data: ArrayBuffer) => void);
    } else {
      this.statusListeners.delete(listener as (s: ConnectionStatus) => void);
    }
  }

  private emit(msg: Uint8Array) {
    const buf = msg.buffer.slice(
      msg.byteOffset,
      msg.byteOffset + msg.byteLength,
    ) as ArrayBuffer;
    for (const l of this.messageListeners) l(buf);
  }

  private setStatus(s: ConnectionStatus) {
    this._status = s;
    for (const l of this.statusListeners) l(s);
  }
}
