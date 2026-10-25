"""Pre-dispatch checks for the two structurally recognized Q8 kernels.

Experimental: these checks do not yet authorize dispatch. The future runtime
must supply real allocation identities, capture buffers after dependency events,
and use the exact immutable snapshots authenticated here. A matching tensor
name or a previous successful check never authorizes later mutable contents.
"""
from dataclasses import dataclass
import hashlib
import json
import re

U64 = (1 << 64) - 1
I32 = (1 << 31) - 1
U32 = (1 << 32) - 1
MAX_BYTES = 512 << 20


def integer(value, maximum=U64):
    if type(value) is not int or not 0 <= value <= maximum:
        raise ValueError('invalid unsigned integer')
    return value


@dataclass(frozen=True)
class BufferView:
    # These values must come from the runtime's allocation registry, not RPC.
    root: int
    root_bytes: int
    offset: int
    size: int

    def interval(self, offset, length):
        for value in (self.root, self.root_bytes, self.offset, self.size, offset, length):
            integer(value)
        if (self.offset + self.size > self.root_bytes or offset + length > self.size
                or self.root_bytes > U64 or length > MAX_BYTES):
            raise ValueError('buffer range outside allocation or admission limit')
        return self.root, self.offset + offset, self.offset + offset + length


@dataclass(frozen=True)
class Launch:
    variant: str
    m: int
    n: int
    k: int
    reads: tuple
    write: tuple


def validate_launch(variant, scalars, buffers, global_size, local_size, offset=(0, 0, 0)):
    """Validate the initial contiguous, non-batched 19-argument ABI subset.

    scalars maps argument index to a decoded int with original ABI size checked
    by the caller. buffers maps 0/1/2/4 to allocation-registry BufferViews.
    Explicit local sizes and a complete zero-offset 3-D grid are required.
    """
    if variant not in ('mm', 'mv'):
        raise ValueError('unknown kernel variant')
    if set(scalars) != {3, 5, *range(6, 19)} or set(buffers) != {0, 1, 2, 4}:
        raise ValueError('incomplete kernel arguments')
    if any(type(key) is not int for key in (*scalars, *buffers)):
        raise ValueError('invalid argument index')
    for values in (global_size, local_size, offset):
        if not isinstance(values, (tuple, list)) or len(values) != 3:
            raise ValueError('expected three dimensions')
        for value in values:
            integer(value)
    if tuple(offset) != (0, 0, 0):
        raise ValueError('global offsets need separate lowering')
    wide = {3, 5} | ({8, 9, 10, 12, 13, 14} if variant == 'mv' else set())
    for index, value in scalars.items():
        integer(value, U64 if index in wide else I32)
    k, n = scalars[6], scalars[7]
    m = scalars[9 if variant == 'mm' else 16]
    if min(k, n, m) == 0 or k % 32 or k > I32 - 128:
        raise ValueError('invalid Q8 dimensions')
    if variant == 'mm':
        expected = {8: 1, 10: 1, 11: k, 12: k, 13: n, 14: k*n,
                    15: k*m, 16: n*m, 17: 1, 18: 1}
        grid, local = ((n+63)//64*128, (m+63)//64, 1), (128, 1, 1)
        # Index arithmetic is signed int in the original kernel, including
        # rounded tile positions and the final pos_a/pos_b increment.
        if max(((n+63)//64*64+32)*k, ((m+63)//64*64+32)*k, m*n) > I32:
            raise ValueError('signed kernel index overflow')
        input_alignment = 16
    else:
        row = k//32*34
        expected = {8: row, 9: row*n, 10: row*n, 11: 1, 12: k*4,
                    13: m*k*4, 14: m*k*4, 15: n, 17: 1, 18: 1}
        grid, local = ((n+7)//8*16, m*2, 1), (16, 2, 1)
        # All four rows are loaded before the output guards. The source also
        # narrows packed row offsets to uint before dividing by 34.
        if n % 8 or n > I32-8 or row*n > U32:
            raise ValueError('unguarded tail rows or narrowed row offset')
        input_alignment = 4
    if any(scalars[index] != value for index, value in expected.items()):
        raise ValueError('unsupported strides, broadcasting or batching')
    if tuple(global_size) != grid or tuple(local_size) != local:
        raise ValueError('incomplete or incompatible workgroup grid')
    if scalars[3] % input_alignment or scalars[5] % 4:
        raise ValueError('unaligned kernel pointer offset')
    roots = {}
    for buffer in buffers.values():
        if not isinstance(buffer, BufferView):
            raise ValueError('allocation registry view required')
        buffer.interval(0, 0)
        if buffer.root in roots and roots[buffer.root] != buffer.root_bytes:
            raise ValueError('inconsistent root allocation size')
        roots[buffer.root] = buffer.root_bytes
    for index, alignment, extra in ((0, 4 if variant == 'mm' else 1, 0),
                                     (1, 2, 0), (2, input_alignment, scalars[3]),
                                     (4, 4, scalars[5])):
        if (buffers[index].offset + extra) % alignment:
            raise ValueError('unaligned sub-buffer address')
    reads = (buffers[0].interval(0, n*k), buffers[1].interval(0, n*k//16),
             buffers[2].interval(scalars[3], m*k*4))
    write = buffers[4].interval(scalars[5], m*n*4)
    if any(write[0] == read[0] and max(write[1], read[1]) < min(write[2], read[2])
           for read in reads):
        raise ValueError('output aliases an input allocation range')
    return Launch(variant, m, n, k, reads, write)


def q8_digest(quants, scales, k, n):
    """Hash original GGUF [half scale, 32 signed-byte codes] block order.

    Keep exact half bits and signed code bytes: dequantizing and requantizing
    would discard distinctions relevant to the approved artifact digest.
    """
    integer(k, I32)
    integer(n, I32)
    if not k or not n or k % 32 or k*n > MAX_BYTES:
        raise ValueError('invalid Q8 tensor shape')
    if type(quants) is not bytes or type(scales) is not bytes:
        raise ValueError('immutable snapshots required')
    if len(quants) != k*n or len(scales) != k*n//16:
        raise ValueError('Q8 tensor lengths differ from shape')
    digest = hashlib.sha256()
    for first in range(0, k*n//32, 4096):
        count = min(4096, k*n//32-first)
        q = quants[first*32:(first+count)*32]
        d = scales[first*2:(first+count)*2]
        interleaved = bytearray(count*34)
        interleaved[0::34], interleaved[1::34] = d[0::2], d[1::2]
        for lane in range(32):
            interleaved[lane+2::34] = q[lane::32]
        digest.update(interleaved)
    return digest.hexdigest()


@dataclass(frozen=True)
class AuthenticatedQ8:
    name: str
    sha256: str
    quants: bytes
    scales: bytes
    k: int
    n: int


class PublicQ8Catalog:
    """Bind public-tensor admission to digests supplied by measured service config.

    The expected digests must never be supplied by an application request.
    Model/calibration contents remain the responsibility of service startup.
    """
    def __init__(self, policy, expected_policy, expected_model, expected_calibration):
        for digest in (expected_policy, expected_model, expected_calibration):
            if not isinstance(digest, str) or not re.fullmatch('[0-9a-f]{64}', digest):
                raise ValueError('invalid trusted digest')
        if type(policy) is not bytes or len(policy) > 4 << 20:
            raise ValueError('invalid policy bytes')
        if hashlib.sha256(policy).hexdigest() != expected_policy:
            raise ValueError('public tensor policy digest mismatch')
        def unique(pairs):
            result = {}
            for key, value in pairs:
                if key in result:
                    raise ValueError('duplicate policy key')
                result[key] = value
            return result
        document = json.loads(policy, object_pairs_hook=unique)
        if not isinstance(document, dict):
            raise ValueError('policy object required')
        if (document.get('schema') != 'shield-public-tensors/1'
                or document.get('modelSha256') != expected_model
                or document.get('calibrationSha256') != expected_calibration):
            raise ValueError('policy binding mismatch')
        entries = document.get('tensors')
        if not isinstance(entries, list) or len(entries) > 100000:
            raise ValueError('invalid tensor list')
        self._entries = {}
        names = set()
        for entry in entries:
            if not isinstance(entry, dict):
                raise ValueError('tensor policy object required')
            name = entry.get('name')
            if (not isinstance(name, str) or not name.isascii() or not 0 < len(name) < 128
                    or any(c.isspace() or ord(c) < 32 for c in name) or name in names):
                raise ValueError('invalid or duplicate tensor name')
            names.add(name)
            if type(entry.get('type')) is not int:
                raise ValueError('invalid tensor type')
            if entry['type'] != 8:  # GGML_TYPE_Q8_0
                continue
            shape = entry.get('ne')
            if not isinstance(shape, list) or len(shape) != 4:
                raise ValueError('invalid Q8 dimensions')
            for dimension in shape:
                integer(dimension, I32)
            k, n, batch, outer = shape
            if (not all(shape) or k % 32 or type(entry.get('nbytes')) is not int
                    or entry['nbytes'] != k*n*batch*outer//32*34):
                raise ValueError('invalid packed Q8 length')
            digest = entry.get('sha256')
            if not isinstance(digest, str) or not re.fullmatch('[0-9a-f]{64}', digest):
                raise ValueError('invalid tensor digest')
            if batch == outer == 1 and k*n <= MAX_BYTES:
                self._entries[name] = (k, n, digest)

    def authenticate(self, name, quants, scales, k, n):
        if not isinstance(name, str):
            raise ValueError('tensor name required')
        entry = self._entries.get(name)
        if entry is None or entry[:2] != (k, n):
            raise ValueError('tensor identity or shape not approved')
        digest = q8_digest(quants, scales, k, n)
        if digest != entry[2]:
            raise ValueError('tensor contents not approved')
        return AuthenticatedQ8(name, digest, quants, scales, k, n)
