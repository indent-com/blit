/** The native host surface installed by Blit's QuickJS extension runtime. */
export interface BlitContext {
  readonly extensionId: bigint;
  readonly definitionRevision: bigint;
  readonly attempt: bigint;
  readonly taskId: number;
  readonly moduleHash: string;
  readonly name?: string | null;
  readonly args: readonly string[];
  readonly detached: boolean;
  readonly persistent: boolean;
  readonly enabled: boolean;
  readonly desiredRunning: boolean;
  readonly protocolVersion: number;
  readonly features: number;
  readonly bootGeneration?: bigint | null;
  readonly serverVersion?: string | null;
}

export interface BlitHost {
  readonly context: BlitContext;
  send(packet: Uint8Array): void;
  recv(): Uint8Array | undefined;
  wait(): 1 | 2;
  waitUntil(deadlineNanos: bigint): 0 | 1 | 2;
  realtimeNow(): bigint;
  monotonicNow(): bigint;
  random(length: number): Uint8Array;
  sleep(milliseconds: number): void;
  log(message: string): void;
}

declare global {
  const blit: BlitHost;
}

/** Encode a JavaScript string without depending on browser-only TextEncoder. */
export function encodeUtf8(text: string): Uint8Array {
  const bytes: number[] = [];
  for (let index = 0; index < text.length; index += 1) {
    let codePoint = text.codePointAt(index) ?? 0xfffd;
    if (codePoint > 0xffff) index += 1;
    if (codePoint >= 0xd800 && codePoint <= 0xdfff) codePoint = 0xfffd;

    if (codePoint <= 0x7f) {
      bytes.push(codePoint);
    } else if (codePoint <= 0x7ff) {
      bytes.push(0xc0 | (codePoint >> 6), 0x80 | (codePoint & 0x3f));
    } else if (codePoint <= 0xffff) {
      bytes.push(
        0xe0 | (codePoint >> 12),
        0x80 | ((codePoint >> 6) & 0x3f),
        0x80 | (codePoint & 0x3f),
      );
    } else {
      bytes.push(
        0xf0 | (codePoint >> 18),
        0x80 | ((codePoint >> 12) & 0x3f),
        0x80 | ((codePoint >> 6) & 0x3f),
        0x80 | (codePoint & 0x3f),
      );
    }
  }
  return Uint8Array.from(bytes);
}

function continuation(byte: number): boolean {
  return (byte & 0xc0) === 0x80;
}

/** Strictly decode UTF-8 without depending on browser-only TextDecoder. */
export function decodeUtf8(bytes: Uint8Array): string {
  const codeUnits: number[] = [];
  for (let index = 0; index < bytes.length; ) {
    const first = bytes[index]!;
    const second = bytes[index + 1] ?? -1;
    const third = bytes[index + 2] ?? -1;
    const fourth = bytes[index + 3] ?? -1;
    let codePoint: number;
    let width: number;

    if (first <= 0x7f) {
      codePoint = first;
      width = 1;
    } else if (
      first >= 0xc2 &&
      first <= 0xdf &&
      index + 1 < bytes.length &&
      continuation(second)
    ) {
      codePoint = ((first & 0x1f) << 6) | (second & 0x3f);
      width = 2;
    } else if (
      first >= 0xe0 &&
      first <= 0xef &&
      index + 2 < bytes.length &&
      continuation(second) &&
      continuation(third) &&
      !(first === 0xe0 && second < 0xa0) &&
      !(first === 0xed && second >= 0xa0)
    ) {
      codePoint =
        ((first & 0x0f) << 12) | ((second & 0x3f) << 6) | (third & 0x3f);
      width = 3;
    } else if (
      first >= 0xf0 &&
      first <= 0xf4 &&
      index + 3 < bytes.length &&
      continuation(second) &&
      continuation(third) &&
      continuation(fourth) &&
      !(first === 0xf0 && second < 0x90) &&
      !(first === 0xf4 && second >= 0x90)
    ) {
      codePoint =
        ((first & 0x07) << 18) |
        ((second & 0x3f) << 12) |
        ((third & 0x3f) << 6) |
        (fourth & 0x3f);
      width = 4;
    } else {
      throw new Error(`invalid UTF-8 at byte ${index}`);
    }

    if (codePoint <= 0xffff) {
      codeUnits.push(codePoint);
    } else {
      const scalar = codePoint - 0x10000;
      codeUnits.push(0xd800 | (scalar >> 10), 0xdc00 | (scalar & 0x3ff));
    }
    index += width;
  }

  let text = "";
  for (let offset = 0; offset < codeUnits.length; offset += 4096) {
    text += String.fromCharCode(...codeUnits.slice(offset, offset + 4096));
  }
  return text;
}

export class ByteWriter {
  readonly #bytes: number[] = [];

  u8(value: number): this {
    this.#bytes.push(value & 0xff);
    return this;
  }

  u16(value: number): this {
    this.#bytes.push(value & 0xff, (value >>> 8) & 0xff);
    return this;
  }

  u32(value: number): this {
    this.#bytes.push(
      value & 0xff,
      (value >>> 8) & 0xff,
      (value >>> 16) & 0xff,
      (value >>> 24) & 0xff,
    );
    return this;
  }

  i32(value: number): this {
    return this.u32(value >>> 0);
  }

  u64(value: bigint): this {
    for (let shift = 0n; shift < 64n; shift += 8n) {
      this.#bytes.push(Number((value >> shift) & 0xffn));
    }
    return this;
  }

  bytes(value: Uint8Array): this {
    for (const byte of value) this.#bytes.push(byte);
    return this;
  }

  text(value: string): this {
    return this.bytes(encodeUtf8(value));
  }

  finish(): Uint8Array {
    return Uint8Array.from(this.#bytes);
  }
}

export class ByteReader {
  #offset = 0;

  constructor(readonly source: Uint8Array) {}

  get remaining(): number {
    return this.source.length - this.#offset;
  }

  u8(): number {
    return this.take(1)[0]!;
  }

  u16(): number {
    const bytes = this.take(2);
    return bytes[0]! + bytes[1]! * 0x100;
  }

  u32(): number {
    const bytes = this.take(4);
    return (
      bytes[0]! +
      bytes[1]! * 0x100 +
      bytes[2]! * 0x10000 +
      bytes[3]! * 0x1000000
    );
  }

  u64(): bigint {
    const bytes = this.take(8);
    let value = 0n;
    for (let index = 7; index >= 0; index -= 1) {
      value = (value << 8n) | BigInt(bytes[index]!);
    }
    return value;
  }

  take(length: number): Uint8Array {
    if (
      !Number.isSafeInteger(length) ||
      length < 0 ||
      length > this.remaining
    ) {
      throw new Error("truncated packet");
    }
    const start = this.#offset;
    this.#offset += length;
    return this.source.subarray(start, this.#offset);
  }

  text(length: number): string {
    return decodeUtf8(this.take(length));
  }

  rest(): Uint8Array {
    return this.take(this.remaining);
  }

  done(): void {
    if (this.remaining !== 0) throw new Error("packet has trailing bytes");
  }
}

export function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
