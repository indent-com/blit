#!/usr/bin/env python3
"""fd-channel example: spawn blit server, pass a client fd via SCM_RIGHTS,
and verify the protocol handshake and initial state burst."""

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


def read_initial_state(sock):
    """Consume state messages through READY and return the required LIST."""
    lst = None
    while True:
        msg = read_frame(sock)
        assert msg, "unexpected empty frame in initial state"
        if msg[0] == S2C_LIST:
            assert lst is None, "received duplicate LIST"
            lst = msg
        elif msg[0] == S2C_READY:
            assert lst is not None, "received READY before LIST"
            return lst


def read_reply(sock, opcode, limit=64):
    """Read frames until one carries `opcode`, skipping unsolicited state.

    A reply is not the next frame. The server pushes state whenever it has
    some — a compositor with a cursor on it sends SURFACE_CURSOR unprompted —
    so anything that waits for a specific answer has to skip past what it did
    not ask for. Bounded so a wrong opcode fails the example rather than
    hanging it.
    """
    for _ in range(limit):
        msg = read_frame(sock)
        assert msg, "unexpected empty frame while waiting for a reply"
        if msg[0] == opcode:
            return msg
    raise AssertionError(f"no 0x{opcode:02x} within {limit} frames")


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

        # The initial burst can include compositor and terminal state around
        # LIST. READY marks its end.
        lst = read_initial_state(client_ours)
        pty_count = struct.unpack_from("<H", lst, 1)[0]
        print(f"LIST: {pty_count} existing PTYs")

        print("READY")

        create_msg = struct.pack("<BHHH", C2S_CREATE, 24, 80, 0)
        write_frame(client_ours, create_msg)

        created = read_reply(client_ours, S2C_CREATED)
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
