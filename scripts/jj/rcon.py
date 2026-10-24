#!/usr/bin/env python3
"""Minimal RCON client for the JJThunder bench server."""
import socket, struct, sys, time

HOST, PORT = "127.0.0.1", 25575
PW = "jjbench2026"

def pkt(rid, ptype, payload: bytes) -> bytes:
    body = struct.pack("<ii", rid, ptype) + payload + b"\x00\x00"
    return struct.pack("<i", len(body)) + body

def recv_pkt(s):
    raw = b""
    while len(raw) < 4:
        c = s.recv(4 - len(raw))
        if not c:
            raise EOFError
        raw += c
    (ln,) = struct.unpack("<i", raw)
    data = b""
    while len(data) < ln:
        c = s.recv(ln - len(data))
        if not c:
            raise EOFError
        data += c
    rid, ptype = struct.unpack("<ii", data[:8])
    return rid, ptype, data[8:-2]

def rcon(cmd: str, timeout=10) -> str:
    s = socket.create_connection((HOST, PORT), timeout=timeout)
    s.sendall(pkt(1, 3, PW.encode()))
    rid, _, _ = recv_pkt(s)
    if rid == -1:
        raise RuntimeError("auth failed")
    s.sendall(pkt(2, 2, cmd.encode()))
    out = []
    s.settimeout(timeout)
    try:
        while True:
            rid, pt, payload = recv_pkt(s)
            out.append(payload.decode("utf-8", "replace"))
            if pt == 2:
                break
    except (socket.timeout, EOFError):
        pass
    s.close()
    return "".join(out)

if __name__ == "__main__":
    print(rcon(" ".join(sys.argv[1:])))
