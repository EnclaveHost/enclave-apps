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
# One buffer range. A 27B model's output layer is about 1 GiB as Q6_K.
MAX_BYTES = 2 << 30


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
    """Validate the initial contiguous, non-batched ABI subset.

    scalars maps argument index to a decoded int with original ABI size checked
    by the caller. buffers maps the weight, input and output argument indices
    to allocation-registry BufferViews: 0/1/2/4 for the Q8_0 kernels. The
    K-quant kernels have the Q8_0 kernels' arguments after four weight buffers
    (five for Q5_K) instead of two, so 0/1/2/3/4/6 or 0/1/2/3/4/5/7.
    Explicit local sizes and a complete zero-offset 3-D grid are required.
    """
    if variant in ('mv_q6_k', 'mv_iq4_nl'):
        return validate_short_vector(variant, scalars, buffers, global_size, local_size, offset)
    if variant in RAW_BLOCK:
        return validate_raw(variant, scalars, buffers, global_size, local_size, offset)
    if variant not in ('mm', 'mv', 'mm_q4_k', 'mv_q4_k', 'mm_q5_k', 'mv_q5_k', 'mm_q6_k', 'mm_iq4_nl'):
        raise ValueError('unknown kernel variant')
    k_quant = variant.endswith('_k')
    # IQ4_NL has the Q8_0 matrix kernel's arguments; its quants are nibbles.
    nibbles = variant == 'mm_iq4_nl'
    q5, q6 = variant.endswith('_q5_k'), variant.endswith('_q6_k')
    matrix = variant.startswith('mm')
    # Every argument after the weights moves by this: Q5_K has a fifth weight
    # buffer, the high bit of each quant, after the quants.
    shift = 3 if q5 else 2 if k_quant else 0
    at = lambda index: index + shift
    weights = tuple(range(2 + shift))
    if (set(scalars) != {at(3), at(5), *range(at(6), at(19))}
            or set(buffers) != {*weights, at(2), at(4)}):
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
    strides = set() if matrix else {at(8), at(9), at(10), at(12), at(13), at(14)}
    # The Q4_K and Q5_K vector kernels take their two pointer offsets as int.
    wide = strides | (set() if variant in ('mv_q4_k', 'mv_q5_k') else {at(3), at(5)})
    for index, value in scalars.items():
        integer(value, U64 if index in wide else I32)
    k, n = scalars[at(6)], scalars[at(7)]
    m = scalars[at(9 if matrix else 16)]
    block = 256 if k_quant else 32
    if min(k, n, m) == 0 or k % block or k > I32 - 128:
        raise ValueError('invalid tensor dimensions')
    if matrix:
        expected = {8: 1, 10: 1, 11: k, 12: k, 13: n, 14: k*n,
                    15: k*m, 16: n*m, 17: 1, 18: 1}
        grid, local = ((n+63)//64*128, (m+63)//64, 1), (128, 1, 1)
        # Index arithmetic is signed int in the original kernel, including
        # rounded tile positions and the final pos_a/pos_b increment.
        if max(((n+63)//64*64+32)*k, ((m+63)//64*64+32)*k, m*n) > I32:
            raise ValueError('signed kernel index overflow')
        input_alignment = 16
    else:
        row = k//256*176 if q5 else k//256*144 if k_quant else k//32*34
        expected = {8: row, 9: row*n, 10: row*n, 11: 1, 12: k*4,
                    13: m*k*4, 14: m*k*4, 15: n, 17: 1, 18: 1}
        if k_quant:
            grid, local = ((n+3)//4*16, m, 1), (16, 1, 1)
            # Four rows are loaded before the output guards. The block index
            # of a row, its byte offset into the quants (128 bytes a block),
            # the input offset and the output index are all signed int.
            if n % 4 or n > I32-4 or n*k//2 > I32 or m*k*4 > I32 or m*n > I32:
                raise ValueError('unguarded tail rows or signed kernel index overflow')
        else:
            grid, local = ((n+7)//8*16, m*2, 1), (16, 2, 1)
            # All four rows are loaded before the output guards. The source also
            # narrows packed row offsets to uint before dividing by 34.
            if n % 8 or n > I32-8 or row*n > U32:
                raise ValueError('unguarded tail rows or narrowed row offset')
        input_alignment = 4
    if any(scalars[at(index)] != value for index, value in expected.items()):
        raise ValueError('unsupported strides, broadcasting or batching')
    if tuple(global_size) != grid or tuple(local_size) != local:
        raise ValueError('incomplete or incompatible workgroup grid')
    if scalars[at(3)] % input_alignment or scalars[at(5)] % 4:
        raise ValueError('unaligned kernel pointer offset')
    roots = {}
    for buffer in buffers.values():
        if not isinstance(buffer, BufferView):
            raise ValueError('allocation registry view required')
        buffer.interval(0, 0)
        if buffer.root in roots and roots[buffer.root] != buffer.root_bytes:
            raise ValueError('inconsistent root allocation size')
        roots[buffer.root] = buffer.root_bytes
    if q5:
        # As Q4_K, with the high bits read a byte at a time.
        aligned = ((0, 4, 0), (1, 1, 0), (2, 1 if matrix else 2, 0), (3, 2, 0), (4, 2, 0))
    elif q6:
        # Quants and scales are read a byte at a time; d is half.
        aligned = ((0, 1, 0), (1, 1, 0), (2, 1, 0), (3, 2, 0))
    elif k_quant:
        # The quants are read through uchar4 (matrix) or uint (vector)
        # pointers; the vector kernel reads the scales as ushort.
        aligned = ((0, 4, 0), (1, 1 if matrix else 2, 0), (2, 2, 0), (3, 2, 0))
    else:
        aligned = ((0, 4 if matrix else 1, 0), (1, 2, 0))  # IQ4_NL reads its quants as uchar4 too
    for index, alignment, extra in (*aligned, (at(2), input_alignment, scalars[at(3)]),
                                    (at(4), 4, scalars[at(5)])):
        if (buffers[index].offset + extra) % alignment:
            raise ValueError('unaligned sub-buffer address')
    if k_quant:
        blocks = n*k//256
        sizes = (128, 32, 12, 2, 2) if q5 else (128, 64, 16, 2) if q6 else (128, 12, 2, 2)
        reads = tuple(buffers[index].interval(0, blocks*size) for index, size in enumerate(sizes))
    else:
        reads = (buffers[0].interval(0, n*k//2 if nibbles else n*k), buffers[1].interval(0, n*k//16))
    reads += (buffers[at(2)].interval(scalars[at(3)], m*k*4),)
    write = buffers[at(4)].interval(scalars[at(5)], m*n*4)
    if any(write[0] == read[0] and max(write[1], read[1]) < min(write[2], read[2])
           for read in reads):
        raise ValueError('output aliases an input allocation range')
    return Launch(variant, m, n, k, reads, write)


def validate_short_vector(variant, scalars, buffers, global_size, local_size, offset=(0, 0, 0)):
    """The Q6_K and IQ4_NL vector kernels, which share a list of arguments after
    their weight buffers (four: low and high quant bits, scales, d; or two:
    quants, d): input, its offset, output, its offset, then ne00, ne01, ne02,
    ne10, ne12, ne0, ne1, r2, r3."""
    q6 = variant == 'mv_q6_k'
    weights = 4 if q6 else 2
    at = lambda index: index + weights
    if (set(scalars) != {at(1), at(3), *range(at(4), at(13))}
            or set(buffers) != {*range(weights), at(0), at(2)}):
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
    for index, value in scalars.items():
        integer(value, U64 if index in (at(1), at(3)) else I32)
    k, n, m = scalars[at(4)], scalars[at(5)], scalars[at(10)]
    if min(k, n, m) == 0 or k % (256 if q6 else 32) or k > I32 - 128:
        raise ValueError('invalid tensor dimensions')
    # Block counts, the input index and the output index are signed int
    # products. The Q6_K kernel guards every row before it reads it; the
    # IQ4_NL kernel reads eight rows and guards only the writes.
    if n > I32 - 8 or n*k//32 > I32 or m*k > I32 or m*n > I32 or (not q6 and n % 8):
        raise ValueError('signed kernel index overflow or unguarded tail rows')
    if any(scalars[at(index)] != value for index, value in
           {6: 1, 7: k, 8: 1, 9: n, 11: 1, 12: 1}.items()):
        raise ValueError('unsupported strides, broadcasting or batching')
    grid, local = (((n+7)//8*16, m*2, 1), (16, 2, 1)) if q6 else (((n+7)//8*16, m, 1), (16, 1, 1))
    if tuple(global_size) != grid or tuple(local_size) != local:
        raise ValueError('incomplete or incompatible workgroup grid')
    if scalars[at(1)] % 4 or scalars[at(3)] % 4:
        raise ValueError('unaligned kernel pointer offset')
    roots = {}
    for buffer in buffers.values():
        if not isinstance(buffer, BufferView):
            raise ValueError('allocation registry view required')
        buffer.interval(0, 0)
        if buffer.root in roots and roots[buffer.root] != buffer.root_bytes:
            raise ValueError('inconsistent root allocation size')
        roots[buffer.root] = buffer.root_bytes
    aligned = ((0, 1), (1, 1), (2, 1), (3, 2)) if q6 else ((0, 1), (1, 2))
    for index, alignment, extra in (*((i, a, 0) for i, a in aligned), (at(0), 4, scalars[at(1)]),
                                    (at(2), 4, scalars[at(3)])):
        if (buffers[index].offset + extra) % alignment:
            raise ValueError('unaligned sub-buffer address')
    if q6:
        blocks = n*k//256
        reads = tuple(buffers[index].interval(0, blocks*size) for index, size in enumerate((128, 64, 16, 2)))
    else:
        reads = (buffers[0].interval(0, n*k//2), buffers[1].interval(0, n*k//16))
    reads += (buffers[at(0)].interval(scalars[at(1)], m*k*4),)
    write = buffers[at(2)].interval(scalars[at(3)], m*n*4)
    if any(write[0] == read[0] and max(write[1], read[1]) < min(write[2], read[2])
           for read in reads):
        raise ValueError('output aliases an input allocation range')
    return Launch(variant, m, n, k, reads, write)


def iq4_nl_digest(quants, scales, k, n):
    """Hash original GGUF block_iq4_nl order: half d, then 16 quant bytes, for
    each 32 weights. Exact bytes."""
    integer(k, I32)
    integer(n, I32)
    if not k or not n or k % 32 or k*n > 2*MAX_BYTES:
        raise ValueError('invalid IQ4_NL tensor shape')
    if type(quants) is not bytes or type(scales) is not bytes:
        raise ValueError('immutable snapshots required')
    blocks = k*n//32
    if len(quants) != blocks*16 or len(scales) != blocks*2:
        raise ValueError('IQ4_NL tensor lengths differ from shape')
    digest = hashlib.sha256()
    for block in range(blocks):
        digest.update(scales[block*2:block*2+2])
        digest.update(quants[block*16:block*16+16])
    return digest.hexdigest()


# Bytes of GGUF for each 256 weights, for the types kept as the file has them.
RAW_BLOCK = {'raw_iq4_xs': 136, 'raw_q3_k': 110, 'raw_iq3_s': 110}


def validate_raw(variant, scalars, buffers, global_size, local_size, offset=(0, 0, 0)):
    """A plain kernel (IQ4_XS, Q3_K, IQ3_S): weights as GGUF bytes and their
    offset, input and offset, output and offset, then ne00, ne01, ne11. One
    work item an output, every read guarded by the two dimensions."""
    if set(scalars) != {1, 3, 5, 6, 7, 8} or set(buffers) != {0, 2, 4}:
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
    for index, value in scalars.items():
        integer(value, U64 if index in (1, 3, 5) else I32)
    k, n, m = scalars[6], scalars[7], scalars[8]
    if min(k, n, m) == 0 or k % 256 or k > I32 - 256:
        raise ValueError('invalid tensor dimensions')
    if tuple(global_size) != (n, m, 1) or tuple(local_size) != (1, 1, 1):
        raise ValueError('incomplete or incompatible workgroup grid')
    roots = {}
    for buffer in buffers.values():
        if not isinstance(buffer, BufferView):
            raise ValueError('allocation registry view required')
        buffer.interval(0, 0)
        if buffer.root in roots and roots[buffer.root] != buffer.root_bytes:
            raise ValueError('inconsistent root allocation size')
        roots[buffer.root] = buffer.root_bytes
    # d is loaded as a half; input and output are float.
    for index, alignment, extra in ((0, 2, scalars[1]), (2, 4, scalars[3]), (4, 4, scalars[5])):
        if (buffers[index].offset + extra) % alignment:
            raise ValueError('unaligned kernel pointer')
    reads = (buffers[0].interval(scalars[1], n*k//256*RAW_BLOCK[variant]), buffers[2].interval(scalars[3], m*k*4))
    write = buffers[4].interval(scalars[5], m*n*4)
    if any(write[0] == read[0] and max(write[1], read[1]) < min(write[2], read[2])
           for read in reads):
        raise ValueError('output aliases an input allocation range')
    return Launch(variant, m, n, k, reads, write)


def q6_k_digest(low, high, scales, d, k, n):
    """Hash original GGUF block_q6_K order: 128 low-bit bytes, 64 high-bit
    bytes, 16 scale bytes, half d for each 256 weights. Exact bytes."""
    integer(k, I32)
    integer(n, I32)
    if not k or not n or k % 256 or k*n > 2*MAX_BYTES:
        raise ValueError('invalid Q6_K tensor shape')
    if any(type(part) is not bytes for part in (low, high, scales, d)):
        raise ValueError('immutable snapshots required')
    blocks = k*n//256
    if (len(low), len(high), len(scales), len(d)) != (blocks*128, blocks*64, blocks*16, blocks*2):
        raise ValueError('Q6_K tensor lengths differ from shape')
    digest = hashlib.sha256()
    for block in range(blocks):
        digest.update(low[block*128:block*128+128])
        digest.update(high[block*64:block*64+64])
        digest.update(scales[block*16:block*16+16])
        digest.update(d[block*2:block*2+2])
    return digest.hexdigest()


def q5_k_digest(quants, high, scales, d, dmin, k, n):
    """Hash original GGUF block_q5_K order: half d, half dmin, 12 scale bytes,
    32 high-bit bytes, 128 quant bytes for each 256 weights. Exact bytes."""
    integer(k, I32)
    integer(n, I32)
    if not k or not n or k % 256 or k*n > 2*MAX_BYTES:
        raise ValueError('invalid Q5_K tensor shape')
    if any(type(part) is not bytes for part in (quants, high, scales, d, dmin)):
        raise ValueError('immutable snapshots required')
    blocks = k*n//256
    if (len(quants), len(high), len(scales), len(d), len(dmin)) != (blocks*128, blocks*32, blocks*12,
                                                                     blocks*2, blocks*2):
        raise ValueError('Q5_K tensor lengths differ from shape')
    digest = hashlib.sha256()
    for block in range(blocks):
        digest.update(d[block*2:block*2+2])
        digest.update(dmin[block*2:block*2+2])
        digest.update(scales[block*12:block*12+12])
        digest.update(high[block*32:block*32+32])
        digest.update(quants[block*128:block*128+128])
    return digest.hexdigest()


def q4_k_digest(quants, scales, d, dmin, k, n):
    """Hash original GGUF block_q4_K order: half d, half dmin, 12 scale bytes,
    128 quant bytes for each 256 weights. Exact bytes, as for Q8."""
    integer(k, I32)
    integer(n, I32)
    if not k or not n or k % 256 or k*n > 2*MAX_BYTES:
        raise ValueError('invalid Q4_K tensor shape')
    if any(type(part) is not bytes for part in (quants, scales, d, dmin)):
        raise ValueError('immutable snapshots required')
    blocks = k*n//256
    if (len(quants), len(scales), len(d), len(dmin)) != (blocks*128, blocks*12, blocks*2, blocks*2):
        raise ValueError('Q4_K tensor lengths differ from shape')
    digest = hashlib.sha256()
    for first in range(0, blocks, 1024):
        count = min(1024, blocks-first)
        interleaved = bytearray(count*144)
        for lane in range(2):
            interleaved[lane::144] = d[first*2+lane:(first+count)*2:2]
            interleaved[2+lane::144] = dmin[first*2+lane:(first+count)*2:2]
        for lane in range(12):
            interleaved[4+lane::144] = scales[first*12+lane:(first+count)*12:12]
        for lane in range(128):
            interleaved[16+lane::144] = quants[first*128+lane:(first+count)*128:128]
        digest.update(interleaved)
    return digest.hexdigest()


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
