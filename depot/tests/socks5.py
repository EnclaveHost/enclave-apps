#!/usr/bin/env python3
"""A SOCKS5 front like the platform's per-deployment egress: user/pass auth
(the deployment id and token), CONNECT only, BND.ADDR answered. Counts the
connections it relays so the test can prove storage traffic went through it.
  python3 tests/socks5.py <port> <user> <pass> <counter-file>"""
import socket
import struct
import sys
import threading

port, user, pw, counter = int(sys.argv[1]), sys.argv[2].encode(), sys.argv[3].encode(), sys.argv[4]
n = 0
lock = threading.Lock()


def pump(a, b):
    try:
        while True:
            d = a.recv(65536)
            if not d:
                break
            b.sendall(d)
    except OSError:
        pass
    finally:
        try:
            b.shutdown(socket.SHUT_WR)
        except OSError:
            pass


def handle(c):
    global n
    try:
        if c.recv(2) != b"\x05\x01" or c.recv(1) != b"\x02":
            return c.close()
        c.sendall(b"\x05\x02")
        hdr = c.recv(2)
        u = c.recv(hdr[1])
        pl = c.recv(1)[0]
        p = c.recv(pl)
        if u != user or p != pw:
            c.sendall(b"\x01\x01")
            return c.close()
        c.sendall(b"\x01\x00")
        ver, cmd, _, atyp = c.recv(4)
        if atyp == 1:
            host = socket.inet_ntoa(c.recv(4))
        elif atyp == 3:
            host = c.recv(c.recv(1)[0]).decode()
        else:
            host = socket.inet_ntop(socket.AF_INET6, c.recv(16))
        (dport,) = struct.unpack(">H", c.recv(2))
        u = socket.create_connection((host, dport))
        c.sendall(b"\x05\x00\x00\x01" + socket.inet_aton("127.0.0.1") + struct.pack(">H", 0))
        with lock:
            n += 1
            open(counter, "w").write(str(n))
        threading.Thread(target=pump, args=(c, u), daemon=True).start()
        pump(u, c)
    except Exception:
        c.close()


s = socket.socket()
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", port))
s.listen(64)
while True:
    conn, _ = s.accept()
    threading.Thread(target=handle, args=(conn,), daemon=True).start()
