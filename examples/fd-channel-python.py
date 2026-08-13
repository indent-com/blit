#!/usr/bin/env python3
"""fd-channel example: spawn blit server, pass a client fd via SCM_RIGHTS,
and verify the protocol handshake (HELLO, LIST, READY, CREATE/CREATED)."""

import os
import signal
import socket
import struct
import subprocess
import sys

BLIT_SERVER = os.environ.get("BLIT_SERVER", "blit")

S2C_HELLO = 0x07
S2C_LIST = 0x03
S2C_READY = 0x09
S2C_CREATED = 0x01
C2S_CREATE = 0x10


def read_frame(sock):
    buf = b""
    while len(buf) < 4:
        chunk = sock.recv(4 - len(buf))
        if not chunk:
            raise ConnectionError("connection closed during length read")
        buf += chunk
    length = int.from_bytes(buf, "little")
    if length == 0:
        return b""
    data = b""
    while len(data) < length:
        chunk = sock.recv(length - len(data))
        if not chunk:
            raise ConnectionError("connection closed during payload read")
        data += chunk
    return data


def write_frame(sock, payload):
    sock.sendall(struct.pack("<I", len(payload)) + payload)


def read_until(sock, opcode, name):
    """Read frames until one carries `opcode`, skipping the rest.

    The pre-READY burst is extensible: a server with a compositor also sends
    clipboard-owner and surface state before the PTY list, and new families
    may add more. Dispatch on the opcode rather than on position, the same way
    a client must ignore opcodes it does not know.
    """
    while True:
        frame = read_frame(sock)
        if not frame:
            raise ConnectionError(f"empty frame while waiting for {name}")
        if frame[0] == opcode:
            return frame
        if frame[0] == S2C_READY:
            raise AssertionError(f"reached READY without seeing {name}")


def main():
    channel_theirs, channel_ours = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

    env = {**os.environ, "BLIT_FD_CHANNEL": str(channel_theirs.fileno())}
    proc = subprocess.Popen(
        [BLIT_SERVER, "server"],
        env=env,
        pass_fds=(channel_theirs.fileno(),),
    )
    channel_theirs.close()

    client_ours, client_theirs = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

    channel_ours.sendmsg(
        [b"\x00"],
        [(socket.SOL_SOCKET, socket.SCM_RIGHTS, struct.pack("i", client_theirs.fileno()))],
    )
    client_theirs.close()

    try:
        hello = read_frame(client_ours)
        assert hello[0] == S2C_HELLO, f"expected HELLO (0x07), got 0x{hello[0]:02x}"
        proto_version = struct.unpack_from("<H", hello, 1)[0]
        print(f"HELLO: protocol version {proto_version}")

        lst = read_until(client_ours, S2C_LIST, "LIST (0x03)")
        pty_count = struct.unpack_from("<H", lst, 1)[0]
        print(f"LIST: {pty_count} existing PTYs")

        read_until(client_ours, S2C_READY, "READY (0x09)")
        print("READY")

        create_msg = struct.pack("<BHHH", C2S_CREATE, 24, 80, 0)
        write_frame(client_ours, create_msg)

        created = read_until(client_ours, S2C_CREATED, "CREATED (0x01)")
        pty_id = struct.unpack_from("<H", created, 1)[0]
        print(f"CREATED: pty_id={pty_id}")

        print("PASS")
    finally:
        client_ours.close()
        channel_ours.close()
        proc.send_signal(signal.SIGTERM)
        proc.wait(timeout=5)


if __name__ == "__main__":
    main()
