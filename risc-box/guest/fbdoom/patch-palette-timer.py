#!/usr/bin/env python3
"""Apply timer-wide.patch to the exact deployed palette binary, keeping layout.

Re-linking moves the renderer away from the app's baked AOT addresses. These
three bounded replacements implement the same wide arithmetic without moving
any symbol. The emulator still checks instructions before using baked code;
changed timer regions use its normal fallback. No host/runtime checks change.
Refuse every other binary. New builds use timer-wide.patch instead.
"""
import hashlib
from pathlib import Path
import struct
import sys

ORIGINAL_SHA = "b96ad681efc98357b6987fa857ebf778e6ac21fd3602b32b9a93d47e3f17ca00"
# RV64GC, little endian. GetAdjustedTime is inlined in NetUpdate/D_StartGameLoop.
# slli/add/slli/sub/div replace their 32-bit word variants. I_GetTime first
# zero-extends the modulo-2^32 elapsed milliseconds, then multiplies/divides
# in 64 bits and retains its original return/stack restoration.
ADJUSTED_OLD = bytes.fromhex("9b173500a99f9b9727009306803e899fbbc7d702")
ADJUSTED_NEW = bytes.fromhex("93173500aa97939727009306803e898fb3c7d702")
ELAPSED_OLD = bytes.fromhex("8d9f1b953700a2703d9d1b1525001d9d3b55c50245618280")
ELAPSED_NEW = bytes.fromhex("8d9f82178193130530023305f5023355c502a27045618280")
PATCHES = [(0x1413a, ADJUSTED_OLD, ADJUSTED_NEW),
           (0x142a4, ADJUSTED_OLD, ADJUSTED_NEW),
           (0x1fd5a, ELAPSED_OLD, ELAPSED_NEW)]


def file_offset(data, addr, size):
    if data[:6] != b"\x7fELF\x02\x01":
        raise ValueError("expected little-endian ELF64")
    phoff = struct.unpack_from("<Q", data, 32)[0]
    entsize, count = struct.unpack_from("<HH", data, 54)
    for n in range(count):
        kind, flags, off, va, _, filesz, _, _ = struct.unpack_from(
            "<IIQQQQQQ", data, phoff + n * entsize)
        if kind == 1 and flags & 1 and va <= addr and addr + size <= va + filesz:
            return off + addr - va
    raise ValueError("patch is not within executable file-backed segment")


def patch(data):
    if hashlib.sha256(data).hexdigest() != ORIGINAL_SHA:
        raise ValueError("not the exact deployed palette build; refusing")
    out = bytearray(data)
    for addr, old, new in PATCHES:
        if len(old) != len(new):
            raise ValueError("patch would change executable layout")
        off = file_offset(data, addr, len(old))
        if data[off:off + len(old)] != old:
            raise ValueError(f"unexpected instructions at {addr:x}")
        out[off:off + len(old)] = new
    return bytes(out)


if __name__ == "__main__":
    if len(sys.argv) != 3 or Path(sys.argv[1]).resolve() == Path(sys.argv[2]).resolve():
        sys.exit("usage: patch-palette-timer.py ORIGINAL NEW (distinct paths)")
    out = patch(Path(sys.argv[1]).read_bytes())
    with Path(sys.argv[2]).open("xb") as f:
        f.write(out)
    Path(sys.argv[2]).chmod(0o755)
    print(hashlib.sha256(out).hexdigest())
