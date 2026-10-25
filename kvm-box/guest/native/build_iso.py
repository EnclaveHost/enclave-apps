#!/usr/bin/env python3
"""Deterministic ISO9660 wrapper with a payload at byte 65536.

The guest never mounts/parses ISO metadata. Its measured boot code maps the
fixed payload extent and dm-verity authenticates those bytes. The container
exists for Hyper-V's DVD backend, which rejects raw non-ISO images.
Format: ECMA-119 (2019), sections 8.4, 9.1 and 9.4; no boot catalog/Joliet/UDF.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import stat
import struct

SECTOR = 2048
PAYLOAD_OFFSET = 32 * SECTOR
MAX_BYTES = 128 << 30
EXTENT_MAX = 0xFFFFF800
STANDARD = 'https://www.ecma-international.org/wp-content/uploads/ECMA-119_4th_edition_june_2019.pdf'

def both16(v): return struct.pack('<H', v) + struct.pack('>H', v)
def both32(v): return struct.pack('<I', v) + struct.pack('>I', v)

def record(name, extent, size, flags):
    length = 33 + len(name) + (len(name) % 2 == 0)
    r = bytearray(length)
    r[0] = length
    r[2:10] = both32(extent)
    r[10:18] = both32(size)
    r[18:25] = bytes([70, 1, 1, 0, 0, 0, 0])
    r[25] = flags
    r[28:32] = both16(1)
    r[32] = len(name)
    r[33:33+len(name)] = name
    return r

def header(size):
    if type(size) is not int or not 0 < size <= MAX_BYTES or size % 4096:
        raise ValueError('payload must be 4096-aligned and at most 128 GiB')
    h = bytearray(PAYLOAD_OFFSET)
    p = bytearray(SECTOR)
    p[:7] = b'\x01CD001\x01'
    p[8:40] = b'ENCLAVE'.ljust(32, b' ')
    p[40:72] = b'NATIVE_RESOURCE'.ljust(32, b' ')
    p[80:88] = both32(32 + size // SECTOR)
    p[120:124] = both16(1)
    p[124:128] = both16(1)
    p[128:132] = both16(SECTOR)
    p[132:140] = both32(10)
    p[140:144] = struct.pack('<I', 18)
    p[148:152] = struct.pack('>I', 19)
    p[156:190] = record(b'\x00', 20, SECTOR, 2)
    p[190:813] = b' ' * (813 - 190)
    p[574:702] = b'ENCLAVE NATIVE RESOURCE WRAPPER'.ljust(128, b' ')
    for offset in [813, 830, 864]:
        p[offset:offset+17] = b'1970010100000000\x00'
    p[847:864] = b'0000000000000000\x00'
    p[881] = 1
    h[16*SECTOR:17*SECTOR] = p
    h[17*SECTOR:17*SECTOR+7] = b'\xffCD001\x01'
    h[18*SECTOR:18*SECTOR+10] = b'\x01\x00' + struct.pack('<I', 20) + struct.pack('<H', 1) + b'\x00\x00'
    h[19*SECTOR:19*SECTOR+10] = b'\x01\x00' + struct.pack('>I', 20) + struct.pack('>H', 1) + b'\x00\x00'
    directory = record(b'\x00', 20, SECTOR, 2) + record(b'\x01', 20, SECTOR, 2)
    remaining, extent = size, 32
    while remaining:
        length = min(remaining, EXTENT_MAX)
        remaining -= length
        directory += record(b'PAYLOAD.BIN;1', extent, length, 0x80 if remaining else 0)
        extent += length // SECTOR
    if len(directory) > SECTOR:
        raise ValueError('payload extent records exceed directory sector')
    h[20*SECTOR:20*SECTOR+len(directory)] = directory
    assert len(h) == PAYLOAD_OFFSET
    return bytes(h)

def wrap(source, destination, expected, header_only=False):
    if len(expected) != 64 or any(c not in '0123456789abcdef' for c in expected):
        raise ValueError('expected SHA256 must be lowercase hex')
    fd = os.open(source, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
    with os.fdopen(fd, 'rb') as src:
        st = os.fstat(src.fileno())
        if not stat.S_ISREG(st.st_mode):
            raise ValueError('regular payload file required')
        prefix = header(st.st_size)
        payload_sha = hashlib.sha256()
        container_sha = hashlib.sha256(prefix)
        # Exclusive creation: never overwrite an existing candidate/input.
        with open(destination, 'xb') as dst:
            try:
                dst.write(prefix)
                total = 0
                while data := src.read(1 << 20):
                    payload_sha.update(data)
                    container_sha.update(data)
                    total += len(data)
                    if total > st.st_size:
                        raise ValueError('payload grew during wrapping')
                    if not header_only:
                        dst.write(data)
                if total != st.st_size or payload_sha.hexdigest() != expected:
                    raise ValueError('payload changed or does not match its pin')
                dst.flush()
                os.fsync(dst.fileno())
                os.fchmod(dst.fileno(), 0o444)
            except BaseException:
                os.unlink(destination)
                raise
    return {'schema': 'native-resource-iso/1', 'payloadOffset': PAYLOAD_OFFSET,
            'payloadBytes': st.st_size, 'payloadSha256': expected,
            'containerBytes': PAYLOAD_OFFSET + st.st_size, 'containerSha256': container_sha.hexdigest(),
            'headerSha256': hashlib.sha256(prefix).hexdigest(), 'headerOnly': header_only,
            'standard': STANDARD, 'guestParsesIsoMetadata': False}

if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('source', type=Path)
    p.add_argument('destination', type=Path)
    p.add_argument('--sha256', required=True)
    p.add_argument('--header-only', action='store_true', help='write only the header; still verify/hash the complete payload and container')
    a = p.parse_args()
    print(json.dumps(wrap(a.source, a.destination, a.sha256, a.header_only), sort_keys=True))
