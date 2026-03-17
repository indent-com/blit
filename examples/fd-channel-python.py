#!/usr/bin/env python3
"""fd-channel example: spawn blit-server, pass a client fd via SCM_RIGHTS,
and verify the protocol handshake (HELLO, LIST, READY, CREATE/CREATED)."""

import os
import signal
import socket
import struct
import subprocess
import sys

BLIT_SERVER = os.environ.get("BLIT_SERVER", "blit-server")

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


def main():
    channel_theirs, channel_ours = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)

    env = {**os.environ, "BLIT_FD_CHANNEL": str(channel_theirs.fileno())}
    proc = subprocess.Popen(
        [BLIT_SERVER],
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

        lst = read_frame(client_ours)
        assert lst[0] == S2C_LIST, f"expected LIST (0x03), got 0x{lst[0]:02x}"
        pty_count = struct.unpack_from("<H", lst, 1)[0]
        print(f"LIST: {pty_count} existing PTYs")

        ready = read_frame(client_ours)
        assert ready[0] == S2C_READY, f"expected READY (0x09), got 0x{ready[0]:02x}"
        print("READY")

        create_msg = struct.pack("<BHHH", C2S_CREATE, 24, 80, 0)
        write_frame(client_ours, create_msg)

        created = read_frame(client_ours)
        assert created[0] == S2C_CREATED, f"expected CREATED (0x01), got 0x{created[0]:02x}"
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
