/**
 * Incrementally decode little-endian u32 length-prefixed frames.
 *
 * Complete frames already present in an input chunk are delivered as views of
 * that chunk. Only a frame split across chunks is copied, into an accumulator
 * retained and reused for later split frames. The callback is synchronous and
 * must not retain its view after returning.
 */
export class LengthPrefixedFrameDecoder {
  private pending = new Uint8Array(0);
  private pendingLength = 0;
  private pendingFrameLength = 0;

  constructor(
    private readonly maxFrameLength: number,
    private readonly onFrame: (frame: Uint8Array) => void,
  ) {}

  /** Returns false for an invalid or oversized frame length. */
  push(chunk: Uint8Array): boolean {
    let offset = 0;

    if (this.pendingLength > 0) {
      if (this.pendingLength < 4) {
        this.ensurePendingCapacity(4);
        const take = Math.min(4 - this.pendingLength, chunk.length);
        this.pending.set(chunk.subarray(0, take), this.pendingLength);
        this.pendingLength += take;
        offset += take;
        if (this.pendingLength < 4) return true;
        const length = frameLength(this.pending, 0);
        if (length < 0 || length > this.maxFrameLength) return false;
        this.pendingFrameLength = 4 + length;
        this.ensurePendingCapacity(this.pendingFrameLength);
      }

      const take = Math.min(
        this.pendingFrameLength - this.pendingLength,
        chunk.length - offset,
      );
      this.pending.set(
        chunk.subarray(offset, offset + take),
        this.pendingLength,
      );
      this.pendingLength += take;
      offset += take;
      if (this.pendingLength < this.pendingFrameLength) return true;

      this.onFrame(this.pending.subarray(4, this.pendingFrameLength));
      this.pendingLength = 0;
      this.pendingFrameLength = 0;
    }

    while (offset < chunk.length) {
      const remaining = chunk.length - offset;
      if (remaining < 4) {
        this.ensurePendingCapacity(4);
        this.pending.set(chunk.subarray(offset), 0);
        this.pendingLength = remaining;
        return true;
      }

      const length = frameLength(chunk, offset);
      if (length < 0 || length > this.maxFrameLength) return false;
      const total = 4 + length;
      if (remaining < total) {
        this.ensurePendingCapacity(total);
        this.pending.set(chunk.subarray(offset), 0);
        this.pendingLength = remaining;
        this.pendingFrameLength = total;
        return true;
      }

      this.onFrame(chunk.subarray(offset + 4, offset + total));
      offset += total;
    }
    return true;
  }

  private ensurePendingCapacity(length: number): void {
    if (this.pending.length >= length) return;
    let capacity = Math.max(4, this.pending.length);
    while (capacity < length) capacity *= 2;
    const next = new Uint8Array(capacity);
    next.set(this.pending.subarray(0, this.pendingLength));
    this.pending = next;
  }
}

function frameLength(bytes: Uint8Array, offset: number): number {
  return (
    bytes[offset] |
    (bytes[offset + 1] << 8) |
    (bytes[offset + 2] << 16) |
    (bytes[offset + 3] << 24)
  );
}
