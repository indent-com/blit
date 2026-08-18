import { describe, expect, it } from "vitest";
import { BlitConnection } from "../BlitConnection";
import { MockTransport } from "./mock-transport";
import type { BlitWasmModule } from "../TerminalStore";
import {
  C2S_EXT_CONTROL,
  C2S_EXT_PUT,
  C2S_EXT_RUN,
  EXT_CONTROL_LIST,
  EXT_CONTROL_REMOVE,
  EXT_FLAG_PERSIST,
  EXT_INFO_LIST,
  EXT_PHASE_NEED_OBJECT,
  EXT_PHASE_RUNNING,
  EXT_PUT_BEGIN,
  EXT_PUT_FINAL,
  EXT_PUT_STATUS_ALREADY_HAVE,
  EXT_RESTART_ALWAYS,
  EXT_RUN_PERSIST,
  EXT_RUN_UPDATE,
  FEATURE_EXTENSION,
  S2C_EXT_INFO,
  S2C_EXT_PUT_STATUS,
  S2C_EXT_STATUS,
  buildExtensionControlMessage,
  buildExtensionRunMessage,
  formatExtensionId,
  parseExtensionMessage,
  parseModuleDigest,
} from "../extension";
import { STATUS_INVALID, STATUS_OK } from "../types";

const DIGEST =
  "2ce9c852e69a2931610d10221bb4855f93333aa1d64eef8bc07e0d3b9e2c804f";

function connect(features = FEATURE_EXTENSION) {
  const transport = new MockTransport();
  const connection = new BlitConnection({
    id: "test",
    transport,
    wasm: {} as BlitWasmModule,
  });
  transport.pushHello(1, features);
  transport.pushReady();
  return { transport, connection };
}

function sentOf(transport: MockTransport, opcode: number): Uint8Array[] {
  return transport.sent.filter((message) => message[0] === opcode);
}

/** A server `EXT_STATUS`, which is how a run or a control answers. */
function status(
  nonce: number,
  code: number,
  phase: number,
  detail = "",
): Uint8Array {
  const detailBytes = new TextEncoder().encode(detail);
  const message = new Uint8Array(99 + detailBytes.length);
  const view = new DataView(message.buffer);
  message[0] = S2C_EXT_STATUS;
  view.setUint16(1, nonce, true);
  message[3] = code;
  message[4] = phase;
  view.setBigUint64(7, 0x1234n, true);
  view.setBigUint64(15, 1n, true);
  message.set(parseModuleDigest(DIGEST)!, 67);
  message.set(detailBytes, 99);
  return message;
}

function putStatus(nonce: number, code: number, received: bigint): Uint8Array {
  const message = new Uint8Array(44);
  const view = new DataView(message.buffer);
  message[0] = S2C_EXT_PUT_STATUS;
  view.setUint16(1, nonce, true);
  message[3] = code;
  message.set(parseModuleDigest(DIGEST)!, 4);
  view.setBigUint64(36, received, true);
  return message;
}

function listReply(nonce: number, name: string): Uint8Array {
  const nameBytes = new TextEncoder().encode(name);
  const message = new Uint8Array(7 + 89 + nameBytes.length);
  const view = new DataView(message.buffer);
  message[0] = S2C_EXT_INFO;
  message[1] = EXT_INFO_LIST;
  view.setUint16(2, nonce, true);
  message[4] = STATUS_OK;
  view.setUint16(5, 1, true);
  const record = 7;
  view.setBigUint64(record, 0xabcdn, true); // extension_id
  view.setBigUint64(record + 8, 3n, true); // definition_revision
  message[record + 16] = EXT_PHASE_RUNNING;
  message[record + 17] = EXT_FLAG_PERSIST;
  message[record + 18] = EXT_RESTART_ALWAYS;
  view.setBigUint64(record + 19, 7n, true); // attempt
  view.setBigUint64(record + 27, 7n, true); // last_running_attempt
  view.setUint32(record + 35, 42, true); // task_id
  view.setBigUint64(record + 39, 9n, true); // output_sequence
  view.setBigUint64(record + 47, 1700n, true); // next_start_unix_ms
  message.set(parseModuleDigest(DIGEST)!, record + 55);
  view.setUint16(record + 87, nameBytes.length, true);
  message.set(nameBytes, record + 89);
  return message;
}

describe("extension wire", () => {
  it("lays a run request out in the server's field order", () => {
    const message = buildExtensionRunMessage({
      nonce: 9,
      flags: EXT_RUN_PERSIST,
      restart: EXT_RESTART_ALWAYS,
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      args: ["--scopes", "user"],
    });
    const view = new DataView(message.buffer);
    expect(message[0]).toBe(C2S_EXT_RUN);
    expect(view.getUint16(1, true)).toBe(9);
    expect(message[3]).toBe(EXT_RUN_PERSIST);
    expect(message[4]).toBe(EXT_RESTART_ALWAYS);
    expect(view.getBigUint64(5, true)).toBe(0n);
    expect(view.getUint16(53, true)).toBe(7);
    expect(new TextDecoder().decode(message.subarray(55, 62))).toBe("systemd");
    // argc, then each argument length-prefixed.
    expect(view.getUint16(62, true)).toBe(2);
    expect(view.getUint32(64, true)).toBe(8);
  });

  it("refuses the control pairs that cannot mean anything", () => {
    expect(() =>
      buildExtensionControlMessage(1, 5n, EXT_CONTROL_LIST),
    ).toThrow();
    expect(() =>
      buildExtensionControlMessage(1, 0n, EXT_CONTROL_REMOVE),
    ).toThrow();
    const list = buildExtensionControlMessage(4, 0n, EXT_CONTROL_LIST);
    expect(list[0]).toBe(C2S_EXT_CONTROL);
    expect(list[11]).toBe(EXT_CONTROL_LIST);
  });

  it("reads a list record, hash and all", () => {
    const parsed = parseExtensionMessage(listReply(3, "systemd"));
    expect(parsed).toMatchObject({ kind: "list", nonce: 3, status: STATUS_OK });
    const record = (parsed as { records: any[] }).records[0];
    expect(record.name).toBe("systemd");
    expect(record.hash).toBe(DIGEST);
    expect(record.phase).toBe(EXT_PHASE_RUNNING);
    expect(formatExtensionId(record.extensionId)).toBe("000000000000abcd");
    expect(record.nextStartUnixMs).toBe(1700n);
  });

  it("only accepts a 64-hex digest", () => {
    expect(parseModuleDigest(DIGEST)).toHaveLength(32);
    expect(parseModuleDigest(DIGEST.toUpperCase())).toHaveLength(32);
    expect(parseModuleDigest("abc")).toBeNull();
    expect(parseModuleDigest(`${DIGEST}00`)).toBeNull();
  });
});

describe("BlitConnection extensions", () => {
  it("refuses to manage extensions the server does not offer", async () => {
    const { connection } = connect(0);
    await expect(connection.listExtensions()).rejects.toThrow(
      /does not support extensions/,
    );
  });

  it("lists what is installed", async () => {
    const { transport, connection } = connect();
    const pending = connection.listExtensions();
    const request = sentOf(transport, C2S_EXT_CONTROL).at(-1)!;
    const nonce = new DataView(request.buffer).getUint16(1, true);
    transport.push(listReply(nonce, "systemd"));
    const records = await pending;
    expect(records.map((record) => record.name)).toEqual(["systemd"]);
  });

  it("uploads the module only when the server asks for it", async () => {
    const { transport, connection } = connect();
    let fetched = 0;
    const install = connection.installExtension({
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      module: async () => {
        fetched++;
        return new Uint8Array([0, 97, 115, 109]);
      },
      restart: EXT_RESTART_ALWAYS,
    });

    let run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    let nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(status(nonce, STATUS_OK, EXT_PHASE_NEED_OBJECT));
    await Promise.resolve();
    await Promise.resolve();

    const put = sentOf(transport, C2S_EXT_PUT).at(-1)!;
    expect(put[3]).toBe(EXT_PUT_BEGIN | EXT_PUT_FINAL);
    const putNonce = new DataView(put.buffer).getUint16(1, true);
    transport.push(putStatus(putNonce, STATUS_OK, 4n));
    await Promise.resolve();
    await Promise.resolve();

    run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(status(nonce, STATUS_OK, EXT_PHASE_RUNNING));
    const result = await install;
    expect(result.phase).toBe(EXT_PHASE_RUNNING);
    expect(fetched).toBe(1);
  });

  it("never fetches the module when the server already has the object", async () => {
    const { transport, connection } = connect();
    let fetched = 0;
    const install = connection.installExtension({
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      module: async () => {
        fetched++;
        return new Uint8Array([0]);
      },
    });
    const run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    const nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(status(nonce, STATUS_OK, EXT_PHASE_RUNNING));
    await install;
    expect(fetched).toBe(0);
    expect(sentOf(transport, C2S_EXT_PUT)).toHaveLength(0);
  });

  it("marks an update with the CAS token it was given", async () => {
    const { transport, connection } = connect();
    void connection.installExtension({
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      module: async () => new Uint8Array([0]),
      expectedExtensionId: 0xabcdn,
      expectedDefinitionRevision: 3n,
    });
    const run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    const view = new DataView(run.buffer);
    expect(run[3] & EXT_RUN_UPDATE).toBe(EXT_RUN_UPDATE);
    expect(view.getBigUint64(5, true)).toBe(0xabcdn);
    expect(view.getBigUint64(13, true)).toBe(3n);
  });

  it("reports a refusal with the server's own detail", async () => {
    const { transport, connection } = connect();
    const install = connection.installExtension({
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      module: async () => new Uint8Array([0]),
    });
    const run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    const nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(
      status(
        nonce,
        STATUS_INVALID,
        0,
        "persistent extension name already exists",
      ),
    );
    await expect(install).rejects.toThrow(/name already exists/);
  });

  it("stops uploading once the server says it already has the object", async () => {
    const { transport, connection } = connect();
    const install = connection.installExtension({
      hash: parseModuleDigest(DIGEST)!,
      name: "systemd",
      module: async () => new Uint8Array(2 * 1024 * 1024),
    });
    let run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    let nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(status(nonce, STATUS_OK, EXT_PHASE_NEED_OBJECT));
    await Promise.resolve();
    await Promise.resolve();
    const put = sentOf(transport, C2S_EXT_PUT).at(-1)!;
    const putNonce = new DataView(put.buffer).getUint16(1, true);
    transport.push(putStatus(putNonce, EXT_PUT_STATUS_ALREADY_HAVE, 0n));
    await Promise.resolve();
    await Promise.resolve();
    // A 2 MiB module is two chunks; the short-circuit means only one went out.
    expect(sentOf(transport, C2S_EXT_PUT)).toHaveLength(1);
    run = sentOf(transport, C2S_EXT_RUN).at(-1)!;
    nonce = new DataView(run.buffer).getUint16(1, true);
    transport.push(status(nonce, STATUS_OK, EXT_PHASE_RUNNING));
    await install;
  });

  it("fails pending requests when the transport drops", async () => {
    const { transport, connection } = connect();
    const pending = connection.listExtensions();
    transport.setStatus("disconnected");
    await expect(pending).rejects.toThrow(/Transport disconnected/);
  });
});
