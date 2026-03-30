#!/usr/bin/env bun
// fd-channel example: spawn blit-server, pass a client fd via SCM_RIGHTS,
// and verify the protocol handshake (HELLO, LIST, READY, CREATE/CREATED).

import { spawn } from "bun";
import { dlopen, FFIType, ptr } from "bun:ffi";

const BLIT_SERVER = process.env.BLIT_SERVER ?? "blit-server";

const DARWIN = process.platform === "darwin";

const libc = dlopen(DARWIN ? "libSystem.B.dylib" : "libc.so.6", {
  socketpair: {
    args: [FFIType.i32, FFIType.i32, FFIType.i32, FFIType.ptr],
    returns: FFIType.i32,
  },
  sendmsg: {
    args: [FFIType.i32, FFIType.ptr, FFIType.i32],
    returns: FFIType.i64,
  },
  close: { args: [FFIType.i32], returns: FFIType.i32 },
  read: { args: [FFIType.i32, FFIType.ptr, FFIType.u64], returns: FFIType.i64 },
  write: {
    args: [FFIType.i32, FFIType.ptr, FFIType.u64],
    returns: FFIType.i64,
  },
  kill: { args: [FFIType.i32, FFIType.i32], returns: FFIType.i32 },
});

const AF_UNIX = 1,
  SOCK_STREAM = 1,
  SCM_RIGHTS = 1;
const SOL_SOCKET = DARWIN ? 0xffff : 1;
const SIGTERM = 15;

const S2C_HELLO = 0x07;
const S2C_LIST = 0x03;
const S2C_READY = 0x09;
const S2C_CREATED = 0x01;
const C2S_CREATE = 0x10;

//   Linux (amd64 & arm64)            Darwin arm64
//   cmsghdr.cmsg_len: size_t (8)     socklen_t (4)
//   CMSG_LEN(4):      20             16
//   CMSG_SPACE(4):    24             16
//   fd data offset:   16             12
//   msghdr size:      56             48
//   msg_iovlen:       size_t (8)     int (4)
//   msg_controllen:   size_t (8)     socklen_t (4)

const CMSG_LEN = DARWIN ? 16 : 20;
const CMSG_SPACE = DARWIN ? 16 : 24;
const CMSG_FD_OFF = DARWIN ? 12 : 16;
const MSGHDR_SIZE = DARWIN ? 48 : 56;

function socketpair(): [number, number] {
  const fds = new Int32Array(2);
  if (libc.symbols.socketpair(AF_UNIX, SOCK_STREAM, 0, ptr(fds)) < 0)
    throw new Error("socketpair failed");
  return [fds[0], fds[1]];
}

function sendFd(channel: number, clientFd: number) {
  const iovBuf = new Uint8Array(1);
  const iov = new BigUint64Array(2);
  iov[0] = BigInt(ptr(iovBuf));
  iov[1] = 1n;

  const cmsg = new DataView(new ArrayBuffer(CMSG_SPACE));
  if (DARWIN) {
    cmsg.setUint32(0, CMSG_LEN, true);
    cmsg.setUint32(4, SOL_SOCKET, true);
    cmsg.setUint32(8, SCM_RIGHTS, true);
  } else {
    cmsg.setBigUint64(0, BigInt(CMSG_LEN), true);
    cmsg.setUint32(8, SOL_SOCKET, true);
    cmsg.setUint32(12, SCM_RIGHTS, true);
  }
  cmsg.setInt32(CMSG_FD_OFF, clientFd, true);

  const msg = new DataView(new ArrayBuffer(MSGHDR_SIZE));
  const iovPtr = BigInt(ptr(new Uint8Array(iov.buffer)));
  const ctrlPtr = BigInt(ptr(new Uint8Array(cmsg.buffer)));

  if (DARWIN) {
    msg.setBigUint64(16, iovPtr, true);
    msg.setUint32(24, 1, true);
    msg.setBigUint64(32, ctrlPtr, true);
    msg.setUint32(40, CMSG_SPACE, true);
  } else {
    msg.setBigUint64(16, iovPtr, true);
    msg.setBigUint64(24, 1n, true);
    msg.setBigUint64(32, ctrlPtr, true);
    msg.setBigUint64(40, BigInt(CMSG_SPACE), true);
  }

  if (
    Number(libc.symbols.sendmsg(channel, ptr(new Uint8Array(msg.buffer)), 0)) <
    0
  )
    throw new Error("sendmsg failed");
}

function readExact(fd: number, size: number): Uint8Array {
  const buf = new Uint8Array(size);
  let offset = 0;
  while (offset < size) {
    const n = Number(
      libc.symbols.read(fd, ptr(buf.subarray(offset)), BigInt(size - offset)),
    );
    if (n <= 0) throw new Error(`read failed (returned ${n})`);
    offset += n;
  }
  return buf;
}

function writeAll(fd: number, data: Uint8Array) {
  let offset = 0;
  while (offset < data.length) {
    const n = Number(
      libc.symbols.write(
        fd,
        ptr(data.subarray(offset)),
        BigInt(data.length - offset),
      ),
    );
    if (n <= 0) throw new Error(`write failed (returned ${n})`);
    offset += n;
  }
}

function readFrame(fd: number): Uint8Array {
  const lenBuf = readExact(fd, 4);
  const len = new DataView(lenBuf.buffer).getUint32(0, true);
  if (len === 0) return new Uint8Array(0);
  return readExact(fd, len);
}

function writeFrame(fd: number, payload: Uint8Array) {
  const frame = new Uint8Array(4 + payload.length);
  new DataView(frame.buffer).setUint32(0, payload.length, true);
  frame.set(payload, 4);
  writeAll(fd, frame);
}

function assert(cond: boolean, msg: string) {
  if (!cond) {
    console.error(`FAIL: ${msg}`);
    process.exit(1);
  }
}

const [channelTheirs, channelOurs] = socketpair();

// Bun's spawn() closes non-stdio fds. Passing Bun.file(fd) at stdio[3]
// makes Bun dup2 the fd to 3 in the child process.
const CHANNEL_FD = 3;

const server = spawn([BLIT_SERVER], {
  env: { ...process.env, BLIT_FD_CHANNEL: String(CHANNEL_FD) },
  stdio: ["inherit", "inherit", "inherit", Bun.file(channelTheirs)],
});
libc.symbols.close(channelTheirs);

const [clientOurs, clientTheirs] = socketpair();
sendFd(channelOurs, clientTheirs);
libc.symbols.close(clientTheirs);

try {
  const hello = readFrame(clientOurs);
  assert(
    hello[0] === S2C_HELLO,
    `expected HELLO (0x07), got 0x${hello[0].toString(16)}`,
  );
  const protoVersion = new DataView(hello.buffer).getUint16(1, true);
  console.log(`HELLO: protocol version ${protoVersion}`);

  const list = readFrame(clientOurs);
  assert(
    list[0] === S2C_LIST,
    `expected LIST (0x03), got 0x${list[0].toString(16)}`,
  );
  const ptyCount = new DataView(list.buffer).getUint16(1, true);
  console.log(`LIST: ${ptyCount} existing PTYs`);

  const ready = readFrame(clientOurs);
  assert(
    ready[0] === S2C_READY,
    `expected READY (0x09), got 0x${ready[0].toString(16)}`,
  );
  console.log("READY");

  const createMsg = new Uint8Array(7);
  const cv = new DataView(createMsg.buffer);
  cv.setUint8(0, C2S_CREATE);
  cv.setUint16(1, 24, true);
  cv.setUint16(3, 80, true);
  cv.setUint16(5, 0, true);
  writeFrame(clientOurs, createMsg);

  const created = readFrame(clientOurs);
  assert(
    created[0] === S2C_CREATED,
    `expected CREATED (0x01), got 0x${created[0].toString(16)}`,
  );
  const ptyId = new DataView(created.buffer).getUint16(1, true);
  console.log(`CREATED: pty_id=${ptyId}`);

  console.log("PASS");
} finally {
  libc.symbols.close(clientOurs);
  libc.symbols.close(channelOurs);
  libc.symbols.kill(server.pid, SIGTERM);
  await server.exited;
}
