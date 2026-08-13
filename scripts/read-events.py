#!/usr/bin/env python3
"""Reads the daemon's event stream and prints the events as JSON.

A minimal WebSocket client, written out rather than pulled in as a dependency, because the
point of this script is to exercise the same path a real client uses: connect, subscribe from
a sequence number, replay history, then read live frames. Anything that read the database
directly would skip the part most likely to be wrong.
"""

import base64
import json
import os
import socket
import struct
import sys
import time


def send_text(sock, text):
    payload = text.encode()
    header = bytearray([0x81])
    n = len(payload)
    mask = os.urandom(4)
    if n < 126:
        header.append(0x80 | n)
    elif n < 65536:
        header.append(0x80 | 126)
        header += struct.pack(">H", n)
    else:
        header.append(0x80 | 127)
        header += struct.pack(">Q", n)
    header += mask
    masked = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))
    sock.sendall(bytes(header) + masked)


def read_text_frames(sock, initial, quiet_after):
    """Collects text frames until the stream has been quiet for `quiet_after` seconds."""
    data = initial
    out = []
    sock.settimeout(0.4)
    last_activity = time.time()
    while time.time() - last_activity < quiet_after:
        progressed = True
        while progressed:
            progressed = False
            if len(data) < 2:
                break
            b1, b2 = data[0], data[1]
            length = b2 & 0x7F
            index = 2
            if length == 126:
                if len(data) < 4:
                    break
                length = struct.unpack(">H", data[2:4])[0]
                index = 4
            elif length == 127:
                if len(data) < 10:
                    break
                length = struct.unpack(">Q", data[2:10])[0]
                index = 10
            if len(data) < index + length:
                break
            payload = data[index : index + length]
            data = data[index + length :]
            progressed = True
            opcode = b1 & 0x0F
            if opcode == 1:
                out.append(payload.decode(errors="replace"))
                last_activity = time.time()
            elif opcode == 8:
                return out
        try:
            chunk = sock.recv(65536)
            if not chunk:
                break
            data += chunk
            last_activity = time.time()
        except socket.timeout:
            pass
    return out


def main():
    port = sys.argv[1] if len(sys.argv) > 1 else "8787"
    since = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    quiet_after = float(sys.argv[3]) if len(sys.argv) > 3 else 1.2

    key = base64.b64encode(os.urandom(16)).decode()
    sock = socket.create_connection(("127.0.0.1", int(port)), timeout=5)
    sock.sendall(
        (
            f"GET /api/stream HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{port}\r\n"
            f"Upgrade: websocket\r\n"
            f"Connection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\n"
            f"Sec-WebSocket-Version: 13\r\n\r\n"
        ).encode()
    )

    buf = b""
    while b"\r\n\r\n" not in buf:
        chunk = sock.recv(4096)
        if not chunk:
            print("[]")
            return
        buf += chunk
    body = buf.split(b"\r\n\r\n", 1)[1]

    send_text(sock, json.dumps({"type": "subscribe", "since_seq": since}))

    events = []
    for frame in read_text_frames(sock, body, quiet_after):
        try:
            msg = json.loads(frame)
        except Exception:
            continue
        if msg.get("type") == "events":
            events.extend(msg.get("events", []))

    try:
        sock.close()
    except Exception:
        pass
    print(json.dumps(events))


if __name__ == "__main__":
    main()
