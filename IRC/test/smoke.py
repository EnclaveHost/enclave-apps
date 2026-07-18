#!/usr/bin/env python3
"""Two-client smoke test for nanircd.

Run the server first (see README), then:  python3 test/smoke.py <port>

Exercises the paths a real IRC client hits: CAP negotiation, registration
numerics, JOIN/NAMES/TOPIC, channel and direct PRIVMSG, MODE +o, NICK change,
KICK, WHOIS, PING and QUIT propagation. Exits 0 on success, 1 with a
transcript on the first failure.
"""

import socket
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 6667
HOST = "127.0.0.1"


class Client:
    def __init__(self, label):
        self.label = label
        self.sock = socket.create_connection((HOST, PORT), timeout=5)
        self.buf = b""
        self.log = []

    def send(self, line):
        self.log.append(f">> {line}")
        self.sock.sendall(line.encode() + b"\r\n")

    def lines(self, deadline):
        while True:
            nl = self.buf.find(b"\n")
            if nl >= 0:
                raw, self.buf = self.buf[: nl + 1], self.buf[nl + 1 :]
                line = raw.decode(errors="replace").rstrip("\r\n")
                self.log.append(f"<< {line}")
                yield line
                continue
            remaining = deadline - time.time()
            if remaining <= 0:
                return
            self.sock.settimeout(remaining)
            try:
                chunk = self.sock.recv(4096)
            except socket.timeout:
                return
            if not chunk:
                return
            self.buf += chunk

    def expect(self, what, timeout=5):
        """Wait for a line containing every token in `what` (str or list)."""
        tokens = [what] if isinstance(what, str) else list(what)
        deadline = time.time() + timeout
        for line in self.lines(deadline):
            if all(t in line for t in tokens):
                return line
        fail(self, f"timed out waiting for {tokens!r}")

    def drain(self, seconds=0.3):
        for _ in self.lines(time.time() + seconds):
            pass


def fail(client, why):
    print(f"FAIL [{client.label}]: {why}\n--- transcript ({client.label}) ---")
    print("\n".join(client.log[-40:]))
    sys.exit(1)


def main():
    print(f"smoke test against {HOST}:{PORT}")

    # --- alice registers through CAP negotiation (the modern-client path) ---
    a = Client("alice")
    a.send("CAP LS 302")
    a.send("NICK alice")
    a.send("USER alice 0 * :Alice Example")
    a.expect(["CAP", "LS"])
    a.send("CAP END")
    a.expect(" 001 alice ")
    a.expect(" 005 alice ")
    a.expect(" 376 alice ")          # end of MOTD
    print("ok: alice registered (with CAP round-trip)")

    # --- bob registers plainly ---
    b = Client("bob")
    b.send("NICK bob")
    b.send("USER bob 0 * :Bob Example")
    b.expect(" 001 bob ")
    b.expect(" 376 bob ")
    print("ok: bob registered")

    # --- channel join, names, join visibility ---
    a.send("JOIN #enclave")
    a.expect(["alice", "JOIN", "#enclave"])
    a.expect([" 353 alice ", "@alice"])   # creator gets +o
    a.expect(" 366 alice ")
    b.send("JOIN #enclave")
    b.expect(["bob", "JOIN", "#enclave"])
    a.expect(["bob", "JOIN", "#enclave"])     # alice sees bob join
    print("ok: JOIN + NAMES + join broadcast")

    # --- topic ---
    a.send("TOPIC #enclave :ephemeral enclave chat")
    a.expect(["TOPIC", "#enclave", "ephemeral enclave chat"])
    b.expect(["TOPIC", "#enclave", "ephemeral enclave chat"])
    print("ok: TOPIC set + broadcast")

    # --- channel message ---
    b.send("PRIVMSG #enclave :hello from bob")
    a.expect(["bob", "PRIVMSG", "#enclave", "hello from bob"])
    print("ok: channel PRIVMSG")

    # --- direct message ---
    a.send("PRIVMSG bob :hi bob, direct")
    b.expect(["alice", "PRIVMSG", "bob", "hi bob, direct"])
    print("ok: direct PRIVMSG")

    # --- op grant then kick ---
    a.send("MODE #enclave +o bob")
    b.expect(["MODE", "#enclave", "+o", "bob"])
    a.send("MODE #enclave")
    a.expect(" 324 alice #enclave ")
    b.send("KICK #enclave alice :testing kick")
    a.expect(["KICK", "#enclave", "alice", "testing kick"])
    a.send("JOIN #enclave")
    a.expect(["alice", "JOIN", "#enclave"])
    print("ok: MODE +o, MODE query, KICK, re-JOIN")

    # --- whois ---
    a.send("WHOIS bob")
    a.expect(" 311 alice bob ")
    a.expect(" 318 alice bob ")
    print("ok: WHOIS")

    # --- nick change visibility ---
    b.send("NICK bobby")
    a.expect(["bob", "NICK", "bobby"])
    print("ok: NICK change broadcast")

    # --- ping ---
    a.send("PING :roundtrip")
    a.expect(["PONG", "roundtrip"])
    print("ok: PING/PONG")

    # --- nick collision ---
    b.send("NICK alice")
    b.expect(" 433 ")
    print("ok: nick collision rejected (433)")

    # --- quit propagation ---
    b.send("QUIT :leaving now")
    a.expect(["bobby", "QUIT", "leaving now"])
    b.expect(["ERROR", "Closing Link"])
    print("ok: QUIT broadcast + ERROR close")

    a.send("QUIT :bye")
    print("\nALL CHECKS PASSED")


if __name__ == "__main__":
    main()
