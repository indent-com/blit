import {
  ByteReader,
  ByteWriter,
  type BlitContext,
  type BlitHost,
  decodeUtf8,
  encodeUtf8,
  errorMessage,
} from "./blit";

const EXT_COMMAND = 0x94;
const EXT_COMMAND_REGISTER = 1;
const EXT_INFO = 0x92;
const EXT_INFO_COMMAND_REGISTERED = 4;
const CHANNEL = 0x95;
const CHANNEL_LISTEN = 1;
const CHANNEL_OPENED = 1;
const CHANNEL_ACCEPTED = 2;
const CHANNEL_DATA = 3;
const CHANNEL_ACK = 4;
const CHANNEL_CLOSE = 5;
const CHANNEL_CLOSED = 5;
const CHANNEL_CLOSE_NORMAL = 0;
const CHANNEL_CLOSE_CANCELLED = 1;
const CHANNEL_MAX_PAYLOAD = 1024 * 1024;
const FEATURE_EXTENSION = 1 << 11;
const FEATURE_CHANNEL = 1 << 12;
const INVOKE = 1;
const INVOKE_STDIN = 1;
const STDOUT = 1;
const STDERR = 2;
const RESULT = 4;
const EXIT = 5;
const LISTENER_ID = 2;
const REGISTER_NONCE = 1;

export interface CommandOption {
  readonly names: readonly string[];
  readonly takes_value?: boolean;
  readonly help?: string;
}

export interface CommandDescription {
  readonly path: readonly string[];
  readonly summary?: string;
  readonly usage?: string;
  readonly options?: readonly CommandOption[];
}

export interface CommandDescriptor {
  readonly protocol: "blit.cli.v1";
  readonly summary: string;
  readonly commands: readonly CommandDescription[];
}

export interface InvocationRequest {
  readonly args: readonly string[];
  readonly streamsStdin: boolean;
}

export type CommandBytes = string | Uint8Array;

export interface CommandResponse {
  readonly stdout?: CommandBytes;
  readonly stderr?: CommandBytes;
  readonly result?: {
    readonly contentType: string;
    readonly data: CommandBytes;
  };
  readonly code?: number;
  readonly detail?: string;
}

export type CommandHandler = (
  request: InvocationRequest,
) => CommandResponse | undefined;

interface ChannelEnvelope {
  readonly kind: number;
  readonly id: number;
  readonly body: Uint8Array;
}

interface AcceptedChannel {
  readonly id: number;
  readonly window: bigint;
}

function bytes(value: CommandBytes): Uint8Array {
  return typeof value === "string" ? encodeUtf8(value) : value;
}

function channelEnvelope(packet: Uint8Array): ChannelEnvelope | undefined {
  if (packet.length < 6 || packet[0] !== CHANNEL) return undefined;
  const reader = new ByteReader(packet);
  reader.u8();
  const kind = reader.u8();
  const id = reader.u32();
  return { kind, id, body: reader.rest() };
}

function listenerName(context: BlitContext): string {
  return `blit.cli.${context.extensionId.toString(16).padStart(16, "0")}.${
    context.attempt
  }`;
}

function listenPacket(name: string): Uint8Array {
  const encoded = encodeUtf8(name);
  if (encoded.length === 0 || encoded.length > 255) {
    throw new Error("command listener name is invalid");
  }
  return new ByteWriter()
    .u8(CHANNEL)
    .u8(CHANNEL_LISTEN)
    .u32(LISTENER_ID)
    .u8(0)
    .u16(encoded.length)
    .bytes(encoded)
    .u32(0)
    .finish();
}

function commandRegisterPacket(descriptor: CommandDescriptor): Uint8Array {
  const source = JSON.stringify(descriptor);
  const encoded = encodeUtf8(source);
  if (encoded.length === 0 || encoded.length > 64 * 1024) {
    throw new Error("command descriptor is empty or exceeds 64 KiB");
  }
  return new ByteWriter()
    .u8(EXT_COMMAND)
    .u8(EXT_COMMAND_REGISTER)
    .u16(REGISTER_NONCE)
    .u32(LISTENER_ID)
    .u32(encoded.length)
    .bytes(encoded)
    .finish();
}

function receive(host: BlitHost): Uint8Array {
  const packet = host.recv();
  if (packet === undefined) throw new Error("extension endpoint closed");
  return packet;
}

function awaitListener(host: BlitHost): void {
  while (true) {
    const packet = receive(host);
    const envelope = channelEnvelope(packet);
    if (envelope?.id !== LISTENER_ID) continue;
    if (envelope.kind === CHANNEL_CLOSED) {
      const reason = envelope.body[0] ?? 0;
      const detail = decodeUtf8(envelope.body.subarray(1));
      throw new Error(
        `command listener closed (${reason})${detail ? `: ${detail}` : ""}`,
      );
    }
    if (envelope.kind !== CHANNEL_OPENED) continue;

    const reader = new ByteReader(envelope.body);
    const status = reader.u8();
    reader.u64();
    reader.text(reader.u16());
    reader.take(reader.u32());
    const detail = decodeUtf8(reader.rest());
    if (status !== 0) {
      throw new Error(
        `command listener failed (${status})${detail ? `: ${detail}` : ""}`,
      );
    }
    return;
  }
}

function awaitRegistration(host: BlitHost): void {
  while (true) {
    const packet = receive(host);
    if (
      packet.length < 21 ||
      packet[0] !== EXT_INFO ||
      packet[1] !== EXT_INFO_COMMAND_REGISTERED
    ) {
      continue;
    }
    const reader = new ByteReader(packet.subarray(2));
    if (reader.u16() !== REGISTER_NONCE) continue;
    const status = reader.u8();
    const extensionId = reader.u64();
    const definitionRevision = reader.u64();
    const detail = decodeUtf8(reader.rest());
    if (status !== 0) {
      throw new Error(
        `command registration failed (${status})${detail ? `: ${detail}` : ""}`,
      );
    }
    if (
      extensionId !== host.context.extensionId ||
      definitionRevision !== host.context.definitionRevision
    ) {
      throw new Error(
        "command registration returned a different extension revision",
      );
    }
    return;
  }
}

function awaitAccepted(host: BlitHost): AcceptedChannel | undefined {
  while (true) {
    const packet = host.recv();
    if (packet === undefined) return undefined;
    const envelope = channelEnvelope(packet);
    if (envelope === undefined) continue;
    if (envelope.id === LISTENER_ID && envelope.kind === CHANNEL_CLOSED)
      return undefined;
    if (envelope.kind !== CHANNEL_ACCEPTED) continue;

    const reader = new ByteReader(envelope.body);
    const acceptedListener = reader.u32();
    const window = reader.u64();
    reader.text(reader.u16());
    reader.take(reader.u32());
    reader.done();
    if (acceptedListener === LISTENER_ID) return { id: envelope.id, window };
  }
}

function awaitInvocation(
  host: BlitHost,
  channelId: number,
): Uint8Array | undefined {
  while (true) {
    const packet = host.recv();
    if (packet === undefined) return undefined;
    const envelope = channelEnvelope(packet);
    if (envelope?.id !== channelId) continue;
    if (envelope.kind === CHANNEL_CLOSED) return undefined;
    if (envelope.kind === CHANNEL_DATA) return envelope.body;
  }
}

export function decodeInvocation(payload: Uint8Array): InvocationRequest {
  const reader = new ByteReader(payload);
  if (reader.u8() !== INVOKE)
    throw new Error("first command message is not INVOKE");
  const flags = reader.u8();
  if ((flags & ~INVOKE_STDIN) !== 0)
    throw new Error("INVOKE uses reserved flags");
  const count = reader.u16();
  const args: string[] = [];
  for (let index = 0; index < count; index += 1) {
    args.push(reader.text(reader.u32()));
  }
  reader.done();
  return { args, streamsStdin: (flags & INVOKE_STDIN) !== 0 };
}

function channelPacket(
  kind: number,
  channelId: number,
  body?: Uint8Array,
): Uint8Array {
  const writer = new ByteWriter().u8(CHANNEL).u8(kind).u32(channelId);
  if (body !== undefined) writer.bytes(body);
  return writer.finish();
}

function sendData(
  host: BlitHost,
  channelId: number,
  payload: Uint8Array,
): void {
  if (payload.length === 0 || payload.length > CHANNEL_MAX_PAYLOAD) {
    throw new Error("command output exceeds the native-channel message limit");
  }
  host.send(channelPacket(CHANNEL_DATA, channelId, payload));
}

function bytePayload(
  kind: number,
  value?: CommandBytes,
): Uint8Array | undefined {
  if (value === undefined) return undefined;
  const data = bytes(value);
  if (data.length === 0) return undefined;
  const payload = new ByteWriter().u8(kind).bytes(data).finish();
  if (payload.length > CHANNEL_MAX_PAYLOAD) {
    throw new Error("command output exceeds the native-channel message limit");
  }
  return payload;
}

function validContentType(value: string): boolean {
  return /^[a-z0-9][a-z0-9!#$&^_.+-]*\/[a-z0-9][a-z0-9!#$&^_.+-]*$/.test(value);
}

function sendResponse(
  host: BlitHost,
  channelId: number,
  window: bigint,
  response: CommandResponse,
): void {
  const payloads: Uint8Array[] = [];
  const stdout = bytePayload(STDOUT, response.stdout);
  const stderr = bytePayload(STDERR, response.stderr);
  if (stdout !== undefined) payloads.push(stdout);
  if (stderr !== undefined) payloads.push(stderr);
  if (response.result !== undefined) {
    if (!validContentType(response.result.contentType)) {
      throw new Error(
        "result content type is not a canonical lowercase media type",
      );
    }
    const contentType = encodeUtf8(response.result.contentType);
    if (contentType.length > 255) {
      throw new Error("result content type exceeds 255 bytes");
    }
    const data = bytes(response.result.data);
    const payload = new ByteWriter()
      .u8(RESULT)
      .u16(contentType.length)
      .bytes(contentType)
      .bytes(data)
      .finish();
    if (payload.length > CHANNEL_MAX_PAYLOAD) {
      throw new Error(
        "command result exceeds the native-channel message limit",
      );
    }
    payloads.push(payload);
  }

  const code = response.code ?? 0;
  if (!Number.isInteger(code) || code < -0x80000000 || code > 0x7fffffff) {
    throw new Error("command exit code is outside i32");
  }
  const detail = encodeUtf8(response.detail ?? "");
  if (detail.length > 4096)
    throw new Error("command exit detail exceeds 4 KiB");
  payloads.push(new ByteWriter().u8(EXIT).i32(code).bytes(detail).finish());
  const total = payloads.reduce(
    (sum, payload) => sum + BigInt(payload.length),
    0n,
  );
  if (total > window) {
    throw new Error(
      "command response exceeds the native-channel credit window",
    );
  }
  for (const payload of payloads) sendData(host, channelId, payload);
  host.send(
    channelPacket(
      CHANNEL_CLOSE,
      channelId,
      new ByteWriter().u8(CHANNEL_CLOSE_NORMAL).finish(),
    ),
  );
}

function cancel(host: BlitHost, channelId: number): void {
  host.send(
    channelPacket(
      CHANNEL_CLOSE,
      channelId,
      new ByteWriter().u8(CHANNEL_CLOSE_CANCELLED).finish(),
    ),
  );
}

/**
 * Register and synchronously serve a small `blit.cli.v1` command surface.
 *
 * Responses below the channel's one-MiB credit window need no extra state,
 * which keeps the common diagnostic/configuration extension path legible.
 */
export function serveCommands(
  descriptor: CommandDescriptor,
  handler: CommandHandler,
  host: BlitHost = blit,
): number {
  const requiredFeatures = FEATURE_EXTENSION | FEATURE_CHANNEL;
  if ((host.context.features & requiredFeatures) !== requiredFeatures) {
    throw new Error(
      "command providers require extension and native-channel support",
    );
  }
  if (!host.context.persistent || !host.context.name) {
    throw new Error("command providers require a named persistent extension");
  }

  host.send(listenPacket(listenerName(host.context)));
  awaitListener(host);
  host.send(commandRegisterPacket(descriptor));
  awaitRegistration(host);

  while (true) {
    const channel = awaitAccepted(host);
    if (channel === undefined) return 0;
    const payload = awaitInvocation(host, channel.id);
    if (payload === undefined) continue;

    try {
      const request = decodeInvocation(payload);
      host.send(
        channelPacket(
          CHANNEL_ACK,
          channel.id,
          new ByteWriter().u64(BigInt(payload.length)).finish(),
        ),
      );
      const response = handler(request) ?? {};
      sendResponse(host, channel.id, channel.window, response);
    } catch (error) {
      try {
        const message = `extension command failed: ${errorMessage(error)}\n`;
        sendResponse(host, channel.id, channel.window, {
          stderr: message,
          code: 1,
        });
      } catch {
        cancel(host, channel.id);
      }
    }
  }
}
