#!/usr/bin/env python3
"""Mock spacedesk server for testing the native client without a Windows host.

Speaks the spacedesk binary protocol (v4.8) over WebSocket and streams synthetic
uncompressed (BGRA) frames, so `spacedesk-cli --compression off` can be validated
end to end:

    python3 native/examples/mock_server.py --port 28252 --frames 60
    native/cli/target/release/spacedesk-cli 127.0.0.1 --compression off \
        --frames 60 --dump /tmp/frames --no-reconnect
"""

import argparse
import base64
import hashlib
import socket
import struct
import threading

HEADER_LEN = 128
PAYLOAD_LEN = 334

T_IDENTIFICATION = 0
T_PING = 1
T_FRAMEBUFFER = 2
T_VISIBILITY = 3
T_CURSORPOS = 4
T_FLOWCTRLACK = 7
T_DISCONNECT = 8
T_MOUSE = 10
T_KEYBOARD = 11

WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"


def ws_accept(key: str) -> str:
    digest = hashlib.sha1((key + WS_GUID).encode()).digest()
    return base64.b64encode(digest).decode()


def recv_exact(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof")
        buf += chunk
    return buf


def read_ws_frame(conn):
    """Returns (fin, opcode, payload)."""
    b = recv_exact(conn, 2)
    fin = b[0] & 0x80
    opcode = b[0] & 0x0F
    masked = b[1] & 0x80
    length = b[1] & 0x7F
    if length == 126:
        length = struct.unpack(">H", recv_exact(conn, 2))[0]
    elif length == 127:
        length = struct.unpack(">Q", recv_exact(conn, 8))[0]
    mask = recv_exact(conn, 4) if masked else None
    payload = bytearray(recv_exact(conn, length))
    if mask:
        for i in range(length):
            payload[i] ^= mask[i % 4]
    return fin, opcode, bytes(payload)


def read_ws_message(conn):
    msg = bytearray()
    opcode0 = None
    while True:
        fin, opcode, payload = read_ws_frame(conn)
        if opcode == 0x9:  # ping -> pong
            send_ws_frame(conn, 0xA, payload)
            continue
        if opcode == 0x8:
            return None
        if opcode0 is None:
            opcode0 = opcode
        msg += payload
        if fin:
            return bytes(msg)


def send_ws_frame(conn, opcode, payload):
    header = bytearray([0x80 | opcode])
    n = len(payload)
    if n < 126:
        header.append(n)
    elif n <= 0xFFFF:
        header.append(126)
        header += struct.pack(">H", n)
    else:
        header.append(127)
        header += struct.pack(">Q", n)
    conn.sendall(bytes(header) + payload)


def build_packet(ty, fields=(), payload=b""):
    header = bytearray(HEADER_LEN)
    struct.pack_into("<I", header, 0, ty)
    struct.pack_into("<I", header, 4, len(payload))
    for off, val in fields:
        struct.pack_into("<I", header, off, val & 0xFFFFFFFF)
    return bytes(header) + payload


def recv_exact_raw(conn, n):
    buf = b""
    while len(buf) < n:
        chunk = conn.recv(n - len(buf))
        if not chunk:
            raise ConnectionError("eof")
        buf += chunk
    return buf


def read_raw_packet(conn):
    header = recv_exact_raw(conn, HEADER_LEN)
    plen = struct.unpack_from("<I", header, 4)[0]
    payload = recv_exact_raw(conn, plen) if plen else b""
    return header + payload


def handle(conn, args):
    if args.tcp:
        # native raw-TCP mode: no HTTP/WebSocket handshake at all
        def read_msg(c):
            return read_raw_packet(c)

        def send_msg(c, data):
            c.sendall(data)
    else:
        # --- websocket handshake ---
        req = b""
        while b"\r\n\r\n" not in req:
            chunk = conn.recv(4096)
            if not chunk:
                return
            req += chunk
        headers = req.decode(errors="replace")
        key = next(
            line.split(":", 1)[1].strip()
            for line in headers.split("\r\n")
            if line.lower().startswith("sec-websocket-key")
        )
        conn.sendall(
            (
                "HTTP/1.1 101 Switching Protocols\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                f"Sec-WebSocket-Accept: {ws_accept(key)}\r\n\r\n"
            ).encode()
        )

        def read_msg(c):
            return read_ws_message(c)

        def send_msg(c, data):
            send_ws_frame(c, 0x2, data)

    # --- identification packet ---
    ident = read_msg(conn)
    if ident is None or len(ident) < HEADER_LEN:
        print("client did not send identification")
        return
    ty, plen = struct.unpack_from("<II", ident, 0)
    major, minor, client_type = struct.unpack_from("<III", ident, 8)
    compression = struct.unpack_from("<I", ident, 24)[0]
    quality = struct.unpack_from("<I", ident, 32)[0]
    res_mode = struct.unpack_from("<I", ident, 48)[0]
    width = struct.unpack_from("<I", ident, 52)[0]
    height = struct.unpack_from("<I", ident, 88)[0]
    cwidth = struct.unpack_from("<I", ident, 56)[0]
    cheight = struct.unpack_from("<I", ident, 92)[0]
    print(
        f"identification: type={ty} payload={plen} proto=v{major}.{minor} "
        f"client_type={client_type} compression={compression} quality={quality} "
        f"res_mode={res_mode} res={width}x{height} custom={cwidth}x{cheight}"
    )
    assert ty == T_IDENTIFICATION, "expected identification packet"

    # --- visibility on ---
    send_msg(conn, build_packet(T_VISIBILITY, [(8, 1)]))

    stop = threading.Event()

    def reader():
        while not stop.is_set():
            try:
                msg = read_msg(conn)
            except (ConnectionError, OSError):
                stop.set()
                return
            if msg is None:
                stop.set()
                return
            ty = struct.unpack_from("<I", msg, 0)[0]
            if ty == T_FLOWCTRLACK:
                if args.verbose:
                    print("ACK received")
            elif ty == T_MOUSE:
                x, y = struct.unpack_from("<II", msg, 8)
                flags = struct.unpack_from("<I", msg, 20)[0]
                print(f"mouse: x={x} y={y} flags=0x{flags:04x}")
            elif ty == T_KEYBOARD:
                vk, sc, flags = struct.unpack_from("<III", msg, 8)
                print(f"key: vk=0x{vk:02x} scan=0x{sc:02x} flags=0x{flags:02x}")
            elif ty == T_DISCONNECT:
                stop.set()
                return

    t = threading.Thread(target=reader, daemon=True)
    t.start()

    # --- stream raw BGRA frames ---
    w, h = args.width, args.height
    for i in range(args.frames):
        if stop.is_set():
            break
        frame = bytearray(w * h * 4)
        for y in range(h):
            for x in range(w):
                o = (y * w + x) * 4
                frame[o] = (x * 255 // max(w - 1, 1)) & 0xFF        # B
                frame[o + 1] = (y * 255 // max(h - 1, 1)) & 0xFF    # G
                frame[o + 2] = ((x + y + i * 4) * 255 // max(w + h - 2, 1)) & 0xFF  # R
                frame[o + 3] = 255
        if args.fragment:
            # split into 4 fragments like the real server does for big frames
            chunk = len(frame) // 4
            for k in range(4):
                part = bytes(frame[k * chunk : (k + 1) * chunk if k < 3 else len(frame)])
                pkt = build_packet(
                    T_FRAMEBUFFER,
                    [(8, w), (12, h), (16, w * 4), (20, 4), (24, 0), (28, 0),
                     (32, w), (36, h), (40, 0), (64, 1)],
                    part,
                )
                send_msg(conn, pkt)
        else:
            pkt = build_packet(
                T_FRAMEBUFFER,
                [(8, w), (12, h), (16, w * 4), (20, 4), (24, 0), (28, 0),
                 (32, w), (36, h), (40, 0), (64, 0)],
                bytes(frame),
            )
            send_msg(conn, pkt)
        threading.Event().wait(args.delay)

    print(f"sent {args.frames} frames, closing")
    try:
        if args.tcp:
            conn.sendall(build_packet(T_DISCONNECT))
        else:
            send_ws_frame(conn, 0x8, struct.pack(">H", 1000))
    except OSError:
        pass
    stop.set()
    conn.close()


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--host", default="127.0.0.1")
    p.add_argument("--port", type=int, default=28252)
    p.add_argument("--width", type=int, default=320)
    p.add_argument("--height", type=int, default=240)
    p.add_argument("--frames", type=int, default=100)
    p.add_argument("--delay", type=float, default=1.0 / 30)
    p.add_argument("--fragment", action="store_true", help="split each frame into 4 fragments")
    p.add_argument("--tcp", action="store_true", help="native raw-TCP mode (no WebSocket), like current drivers expect")
    p.add_argument("-v", "--verbose", action="store_true")
    args = p.parse_args()

    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((args.host, args.port))
    srv.listen(4)
    print(f"mock spacedesk server on {args.host}:{args.port} ({args.width}x{args.height})")
    while True:
        conn, addr = srv.accept()
        print(f"client connected: {addr}")
        try:
            handle(conn, args)
        except Exception as e:  # noqa: BLE001
            print(f"connection error: {e}")


if __name__ == "__main__":
    main()
