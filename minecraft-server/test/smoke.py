#!/usr/bin/env python3
"""End-to-end smoke test for nanmc (Minecraft protocol 47 / 1.8.9).

Speaks the real wire protocol as a headless client, strictly:
  1. Server-list ping (status JSON + pong roundtrip).
  2. Alice logs in: login success, join game (creative), position/look,
     chunk data — every chunk payload is length-validated against its
     section mask, and chunk (0,0) is parsed to find real ground height.
  3. Bob logs in: both see each other (spawn player packets).
  4. Chat: Alice -> broadcast -> Bob.
  5. Movement: Alice moves, Bob receives an entity teleport for her eid.
  6. Blocks: Alice places stone on measured ground (block change reaches
     both, at the exact position/state), Bob breaks it back to air.
  7. Keep-alives are answered whenever they arrive (counted).
  8. Alice quits: Bob sees destroy-entities for her eid and chat still works.

Usage: python3 test/smoke.py [port]   (default 15565)
Exits 0 on pass, 1 with a message on the first violation.
"""

import json
import socket
import struct
import sys
import time

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 15565
HOST = "127.0.0.1"
PROTOCOL = 47
DECORATION = {(31 << 4) | 1, 37 << 4, 38 << 4}  # tall grass, dandelion, poppy


class Fail(Exception):
    pass


def varint(v):
    v &= 0xFFFFFFFF
    out = b""
    while True:
        b = v & 0x7F
        v >>= 7
        if v:
            out += bytes([b | 0x80])
        else:
            return out + bytes([b])


def pack_pos(x, y, z):
    return ((x & 0x3FFFFFF) << 38) | ((y & 0xFFF) << 26) | (z & 0x3FFFFFF)


def unpack_pos(v):
    x = (v >> 38) & 0x3FFFFFF
    y = (v >> 26) & 0xFFF
    z = v & 0x3FFFFFF
    if x >= 1 << 25:
        x -= 1 << 26
    if z >= 1 << 25:
        z -= 1 << 26
    return x, y, z


class Reader:
    def __init__(self, data):
        self.b = data
        self.p = 0

    def take(self, n):
        if self.p + n > len(self.b):
            raise Fail("packet truncated")
        v = self.b[self.p:self.p + n]
        self.p += n
        return v

    def varint(self):
        v = 0
        for i in range(5):
            (b,) = self.take(1)
            v |= (b & 0x7F) << (7 * i)
            if not b & 0x80:
                if v >= 1 << 31:
                    v -= 1 << 32
                return v
        raise Fail("varint too long")

    def string(self):
        return self.take(self.varint()).decode()

    def u8(self):
        return self.take(1)[0]

    def u16(self):
        return struct.unpack(">H", self.take(2))[0]

    def i32(self):
        return struct.unpack(">i", self.take(4))[0]

    def i64(self):
        return struct.unpack(">q", self.take(8))[0]

    def f32(self):
        return struct.unpack(">f", self.take(4))[0]

    def f64(self):
        return struct.unpack(">d", self.take(8))[0]

    def u64(self):
        return struct.unpack(">Q", self.take(8))[0]

    def rest(self):
        return len(self.b) - self.p


class Client:
    def __init__(self, name):
        self.name = name
        self.sock = socket.create_connection((HOST, PORT), timeout=10)
        self.buf = b""
        self.eid = None
        self.keepalives = 0
        self.chunks_ok = 0
        self.chunk_store = {}  # (cx,cz) -> (mask, block-data bytes)

    def send_packet(self, pid, payload=b""):
        body = varint(pid) + payload
        self.sock.sendall(varint(len(body)) + body)

    def recv_packet(self, timeout=10.0):
        deadline = time.monotonic() + timeout
        while True:
            length, used = None, 0
            for i in range(min(5, len(self.buf))):
                b = self.buf[i]
                length = (length or 0) | ((b & 0x7F) << (7 * i))
                if not b & 0x80:
                    used = i + 1
                    break
            else:
                length = None
            if length is not None and len(self.buf) >= used + length:
                frame = self.buf[used:used + length]
                self.buf = self.buf[used + length:]
                r = Reader(frame)
                return r.varint(), r
            budget = deadline - time.monotonic()
            if budget <= 0:
                raise Fail(f"{self.name}: timeout waiting for packet")
            self.sock.settimeout(budget)
            try:
                data = self.sock.recv(65536)
            except socket.timeout:
                raise Fail(f"{self.name}: timeout waiting for packet")
            if not data:
                raise Fail(f"{self.name}: server closed connection")
            self.buf += data

    def side_packet(self, pid, r):
        """Handle packets every client must service regardless of intent."""
        if pid == 0x00:  # keep-alive
            self.send_packet(0x00, varint(r.varint()))
            self.keepalives += 1
            return True
        if pid == 0x21:  # chunk data: validate strictly, keep near-spawn ones
            cx, cz = r.i32(), r.i32()
            cont, mask = r.u8(), r.u16()
            size = r.varint()
            if r.rest() != size:
                raise Fail(f"chunk ({cx},{cz}): declared size {size} != payload {r.rest()}")
            nsec = bin(mask).count("1")
            expected = nsec * (8192 + 2048 + 2048) + (256 if cont else 0)
            if size != expected:
                raise Fail(f"chunk ({cx},{cz}): size {size} != expected {expected} (mask {mask:#x})")
            if nsec == 0:
                return True  # unload
            data = r.take(size)
            state = data[0] | (data[1] << 8)
            if state != 7 << 4:
                raise Fail(f"chunk ({cx},{cz}): block (0,0,0) is {state}, want bedrock 112")
            self.chunks_ok += 1
            if abs(cx) <= 1 and abs(cz) <= 1:
                self.chunk_store[(cx, cz)] = (mask, data[:nsec * 8192])
            return True
        return False

    def expect(self, pid, timeout=10.0, describe=""):
        deadline = time.monotonic() + timeout
        while True:
            got, r = self.recv_packet(max(0.05, deadline - time.monotonic()))
            if self.side_packet(got, r):
                continue
            if got == pid:
                return r
            if time.monotonic() > deadline:
                raise Fail(f"{self.name}: expected 0x{pid:02x} {describe}, last saw 0x{got:02x}")

    def handshake(self, next_state):
        host = HOST.encode()
        self.send_packet(0x00, varint(PROTOCOL) + varint(len(host)) + host +
                         struct.pack(">H", PORT) + varint(next_state))

    def login(self):
        self.handshake(2)
        n = self.name.encode()
        self.send_packet(0x00, varint(len(n)) + n)
        pid, r = self.recv_packet()
        if pid == 0x00:
            raise Fail(f"{self.name}: kicked during login: {r.string()}")
        if pid != 0x02:
            raise Fail(f"{self.name}: expected login success, got 0x{pid:02x}")
        uuid, uname = r.string(), r.string()
        assert_eq(uname, self.name, "login success echoes name")
        if len(uuid) != 36:
            raise Fail(f"bad uuid string {uuid!r}")

        r = self.expect(0x01, describe="join game")
        self.eid = r.i32()
        assert_eq(r.u8(), 1, "creative gamemode")

        r = self.expect(0x08, describe="position/look")
        self.x, self.y, self.z = r.f64(), r.f64(), r.f64()
        if not (0 < self.y < 256):
            raise Fail(f"spawn y {self.y} out of world")
        if self.chunks_ok < 9:
            raise Fail(f"only {self.chunks_ok} chunks arrived before position/look")
        return uuid

    def ground_height(self, x, z):
        """Topmost solid block y in a captured chunk column (world coords)."""
        key = (x // 16, z // 16)
        if key not in self.chunk_store:
            raise Fail(f"chunk {key} not captured")
        mask, blocks = self.chunk_store[key]
        nsec = bin(mask).count("1")
        lx, lz = x % 16, z % 16
        for y in range(nsec * 16 - 1, -1, -1):
            off = (y // 16) * 8192 + 2 * (((y % 16) * 16 + lz) * 16 + lx)
            state = blocks[off] | (blocks[off + 1] << 8)
            if state != 0 and state not in DECORATION:
                return y
        raise Fail(f"no ground found in column ({x},{z})")

    def move_to(self, x, y, z):
        self.x, self.y, self.z = x, y, z
        self.send_packet(0x04, struct.pack(">dddB", x, y, z, 1))

    def chat(self, msg):
        m = msg.encode()
        self.send_packet(0x01, varint(len(m)) + m)

    def place_stone(self, x, y, z):
        """Click the top face of solid block (x,y,z) holding stone."""
        slot = struct.pack(">hBhB", 1, 1, 0, 0)  # id=1, count=1, dmg=0, no NBT
        self.send_packet(0x08, struct.pack(">Q", pack_pos(x, y, z)) +
                         struct.pack(">b", 1) + slot + b"\x08\x10\x08")

    def dig(self, x, y, z):
        self.send_packet(0x07, b"\x00" + struct.pack(">Q", pack_pos(x, y, z)) + b"\x00")

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


def assert_eq(got, want, what):
    if got != want:
        raise Fail(f"{what}: got {got!r}, want {want!r}")


def expect_block_change(cli, x, y, z, state, timeout=10.0):
    deadline = time.monotonic() + timeout
    while True:
        r = cli.expect(0x23, timeout=max(0.05, deadline - time.monotonic()),
                       describe="block change")
        px, py, pz = unpack_pos(r.u64())
        got_state = r.varint()
        if (px, py, pz) == (x, y, z):
            assert_eq(got_state, state, f"block state at ({x},{y},{z})")
            return


def main():
    # --- 1. status ping -----------------------------------------------------
    s = Client("status")
    s.handshake(1)
    s.send_packet(0x00)
    pid, r = s.recv_packet()
    assert_eq(pid, 0x00, "status response id")
    st = json.loads(r.string())
    assert_eq(st["version"]["protocol"], 47, "status protocol")
    assert_eq(st["players"]["online"], 0, "initial online count")
    s.send_packet(0x01, struct.pack(">q", 0x1DEA))
    pid, r = s.recv_packet()
    assert_eq(pid, 0x01, "pong id")
    assert_eq(r.i64(), 0x1DEA, "pong payload")
    s.close()
    print("ok  status ping")

    # --- 2. Alice logs in -----------------------------------------------------
    alice = Client("Alice")
    alice.login()
    print(f"ok  Alice joined (eid {alice.eid}) at "
          f"({alice.x},{alice.y},{alice.z}); {alice.chunks_ok} chunks validated")

    # --- 3. Bob logs in, mutual visibility -------------------------------------
    bob = Client("Bob")
    bob.login()
    r = bob.expect(0x0C, describe="spawn player (Alice)")
    assert_eq(r.varint(), alice.eid, "Bob sees Alice's eid")
    r = alice.expect(0x0C, describe="spawn player (Bob)")
    assert_eq(r.varint(), bob.eid, "Alice sees Bob's eid")
    print(f"ok  mutual spawn (Bob eid {bob.eid})")

    # --- 4. chat -----------------------------------------------------------------
    alice.chat("hello from the enclave")
    deadline = time.monotonic() + 10
    while True:
        r = bob.expect(0x02, timeout=max(0.05, deadline - time.monotonic()),
                       describe="chat")
        payload = r.string()
        if "hello from the enclave" in payload and "Alice" in payload:
            break
    print("ok  chat broadcast")

    # --- 5. movement sync ---------------------------------------------------------
    alice.move_to(alice.x + 3.0, alice.y, alice.z + 2.0)
    deadline = time.monotonic() + 10
    while True:
        r = bob.expect(0x18, timeout=max(0.05, deadline - time.monotonic()),
                       describe="entity teleport")
        if r.varint() != alice.eid:
            continue
        fx, fy, fz = r.i32(), r.i32(), r.i32()
        assert_eq(fx, int(alice.x * 32), "teleport fixed-point x")
        assert_eq(fz, int(alice.z * 32), "teleport fixed-point z")
        break
    print("ok  movement relayed")

    # --- 6. block place + break -----------------------------------------------------
    bx, bz = int(alice.x) + 2, int(alice.z)
    ground = alice.ground_height(bx, bz)
    ty = ground + 1  # place on top of measured ground
    alice.move_to(bx - 1.5, float(ty), bz + 0.5)  # stand beside the spot
    bob.move_to(bx + 1.5, float(ty), bz + 0.5)
    alice.place_stone(bx, ground, bz)
    expect_block_change(alice, bx, ty, bz, 1 << 4)
    expect_block_change(bob, bx, ty, bz, 1 << 4)
    bob.dig(bx, ty, bz)
    expect_block_change(alice, bx, ty, bz, 0)
    print(f"ok  block place/break synced (ground y={ground})")

    # --- 7/8. Alice quits; Bob keeps playing ------------------------------------------
    alice.close()
    deadline = time.monotonic() + 15
    while True:
        pid, r = bob.recv_packet(max(0.05, deadline - time.monotonic()))
        if bob.side_packet(pid, r):
            continue
        if pid == 0x13:  # destroy entities
            eids = [r.varint() for _ in range(r.varint())]
            if alice.eid in eids:
                break
    print("ok  quit propagated (destroy entities)")

    bob.chat("still here")
    deadline = time.monotonic() + 10
    while True:
        r = bob.expect(0x02, timeout=max(0.05, deadline - time.monotonic()),
                       describe="own chat echo")
        if "still here" in r.string():
            break
    bob.close()
    print("PASS: all smoke checks passed "
          f"(keepalives answered: alice={alice.keepalives} bob={bob.keepalives})")


if __name__ == "__main__":
    try:
        main()
    except Fail as e:
        print(f"FAIL: {e}")
        sys.exit(1)
