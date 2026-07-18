#!/usr/bin/env python3
"""Soak test: chunk streaming, retargeting and keep-alives.

Carol logs in, waits for the full view-distance ring (121 chunks at view
distance 5), then hikes ~12 chunks east. Verifies: new terrain streams in as
she crosses chunk borders, far chunks get unload packets, at least one
keep-alive roundtrip happens, and the server still answers chat at the end.

Usage: python3 test/walk.py [port]   (default 15565)
"""

import struct
import sys
import time

sys.path.insert(0, __file__.rsplit("/", 1)[0])
from smoke import Client, Fail, varint  # reuse the strict protocol client

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 15565
VIEW = 5
RING = (2 * VIEW + 1) ** 2


def pump(cli, seconds, chunks=None, unloads=None):
    """Service the connection for a while, tallying chunk/unload packets."""
    deadline = time.monotonic() + seconds
    while True:
        budget = deadline - time.monotonic()
        if budget <= 0:
            return
        try:
            pid, r = cli.recv_packet(budget)
        except Fail as e:
            if "timeout" in str(e):
                return
            raise
        if pid == 0x21 and (chunks is not None or unloads is not None):
            cx, cz = r.i32(), r.i32()
            r.u8()
            mask = r.u16()
            size = r.varint()
            if r.rest() != size:
                raise Fail(f"chunk ({cx},{cz}): size mismatch")
            if mask == 0:
                if unloads is not None:
                    unloads.add((cx, cz))
            elif chunks is not None:
                chunks.add((cx, cz))
            continue
        cli.side_packet(pid, r)


def main():
    import smoke
    smoke.PORT = PORT

    carol = Client("Carol")
    carol.login()
    seen, unloaded = set(), set()

    # Full ring streams in a few ticks (6 chunks/tick, 25ms idle tick).
    pump(carol, 8.0, seen)
    if len(seen) + carol.chunks_ok < RING:
        raise Fail(f"expected {RING} chunks around spawn, got {len(seen) + carol.chunks_ok}")
    print(f"ok  full view ring streamed ({RING} chunks)")

    # Hike east ~12 chunks, one chunk border per second.
    x0, z0 = carol.x, carol.z
    for step in range(1, 13):
        carol.move_to(x0 + step * 16.0, 80.0, z0)
        pump(carol, 1.0, seen, unloaded)
    pump(carol, 6.0, seen, unloaded)

    east = {c for c in seen if c[0] >= 10}
    if len(east) < 30:
        raise Fail(f"too little new terrain streamed while walking east ({len(east)})")
    behind = {c for c in unloaded if c[0] <= 3}
    if len(behind) < 30:
        raise Fail(f"too few chunks unloaded behind the player ({len(behind)})")
    print(f"ok  retarget while walking (new east chunks: {len(east)}, unloads: {len(behind)})")

    # Stick around past the keep-alive interval.
    waited = 0.0
    while carol.keepalives == 0 and waited < 25.0:
        pump(carol, 1.0, seen, unloaded)
        waited += 1.0
    if carol.keepalives == 0:
        raise Fail("no keep-alive within 25s")
    print(f"ok  keep-alive roundtrip ({carol.keepalives} answered)")

    carol.chat("done hiking")
    deadline = time.monotonic() + 10
    while True:
        r = carol.expect(0x02, timeout=max(0.05, deadline - time.monotonic()),
                         describe="chat echo")
        if "done hiking" in r.string():
            break
    carol.close()
    print("PASS: walk soak passed")


if __name__ == "__main__":
    try:
        main()
    except Fail as e:
        print(f"FAIL: {e}")
        sys.exit(1)
