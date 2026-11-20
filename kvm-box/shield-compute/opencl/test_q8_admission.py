import hashlib
import json
import struct
import unittest
from dataclasses import replace

from q8_admission import (BufferView, PublicQ8Catalog, q4_k_digest, q5_k_digest, q6_k_digest, q8_digest,
                          validate_launch)


def fixture(variant, k=96, n=72, m=3):
    scalars = {3: 32, 5: 16, 6: k, 7: n, 17: 1, 18: 1}
    if variant == 'mm':
        scalars.update({8: 1, 9: m, 10: 1, 11: k, 12: k, 13: n,
                        14: k*n, 15: k*m, 16: n*m})
        grid, local = ((n+63)//64*128, (m+63)//64, 1), (128, 1, 1)
    else:
        row = k//32*34
        scalars.update({8: row, 9: row*n, 10: row*n, 11: 1, 12: 4*k,
                        13: 4*k*m, 14: 4*k*m, 15: n, 16: m})
        grid, local = ((n+7)//8*16, 2*m, 1), (16, 2, 1)
    sizes = {0: n*k, 1: n*k//16, 2: m*k*4+32, 4: m*n*4+16}
    buffers = {index: BufferView(index+1, size+64, 64, size) for index, size in sizes.items()}
    return scalars, buffers, grid, local


def k_fixture(variant, k=512, n=72, m=3):
    """The Q4_K kernels: the Q8 arguments two places later, four weight buffers."""
    scalars = {5: 32, 7: 16, 8: k, 9: n, 19: 1, 20: 1}
    if variant == 'mm_q4_k':
        scalars.update({10: 1, 11: m, 12: 1, 13: k, 14: k, 15: n,
                        16: k*n, 17: k*m, 18: n*m})
        grid, local = ((n+63)//64*128, (m+63)//64, 1), (128, 1, 1)
    else:
        row = k//256*144
        scalars.update({10: row, 11: row*n, 12: row*n, 13: 1, 14: 4*k,
                        15: 4*k*m, 16: 4*k*m, 17: n, 18: m})
        grid, local = ((n+3)//4*16, m, 1), (16, 1, 1)
    blocks = n*k//256
    sizes = {0: blocks*128, 1: blocks*12, 2: blocks*2, 3: blocks*2, 4: m*k*4+32, 6: m*n*4+16}
    buffers = {index: BufferView(index+1, size+64, 64, size) for index, size in sizes.items()}
    return scalars, buffers, grid, local


class Q4KLaunchTests(unittest.TestCase):
    def test_real_layouts(self):
        for variant, k, n, m in [('mm_q4_k', 2048, 11008, 32), ('mv_q4_k', 2048, 11008, 1),
                                  ('mm_q4_k', 256, 73, 67), ('mv_q4_k', 256, 4, 3)]:
            with self.subTest(variant=variant, shape=(k, n, m)):
                launch = validate_launch(variant, *k_fixture(variant, k, n, m))
                self.assertEqual((launch.k, launch.n, launch.m), (k, n, m))
                self.assertEqual(len(launch.reads), 5)
                self.assertEqual(launch.reads[0][2]-launch.reads[0][1], k*n//2)
                self.assertEqual(launch.reads[1][2]-launch.reads[1][1], k*n//256*12)
                self.assertEqual(launch.write[2]-launch.write[1], m*n*4)

    def test_every_argument_is_checked_and_q8_arguments_are_not_accepted(self):
        for variant in ('mm_q4_k', 'mv_q4_k'):
            scalars, buffers, grid, local = k_fixture(variant)
            for index in range(8, 21):
                with self.subTest(variant=variant, index=index):
                    with self.assertRaises(ValueError):
                        validate_launch(variant, {**scalars, index: scalars[index]+1}, buffers, grid, local)
            for missing in (*scalars, *buffers):
                with self.subTest(variant=variant, missing=missing):
                    with self.assertRaises(ValueError):
                        validate_launch(variant, {i: v for i, v in scalars.items() if i != missing},
                                        {i: v for i, v in buffers.items() if i != missing}, grid, local)
            with self.assertRaises(ValueError):
                validate_launch(variant, *fixture(variant[:2]))
            with self.assertRaises(ValueError):
                validate_launch(variant[:2], scalars, buffers, grid, local)
            with self.assertRaises(ValueError):
                validate_launch(variant, scalars, buffers, (grid[0]+16, grid[1], 1), local)

    def test_block_size_tail_rows_alignment_and_overflow(self):
        with self.assertRaises(ValueError):  # 32-wide blocks are not K-quant super-blocks
            validate_launch('mv_q4_k', *k_fixture('mv_q4_k', 288, 8, 1))
        with self.assertRaises(ValueError):  # four rows are loaded unguarded
            validate_launch('mv_q4_k', *k_fixture('mv_q4_k', 256, 6, 1))
        with self.assertRaises(ValueError):  # signed byte offset into the quants
            validate_launch('mv_q4_k', *k_fixture('mv_q4_k', 65536, 65536, 1))
        with self.assertRaises(ValueError):
            validate_launch('mm_q4_k', *k_fixture('mm_q4_k', 1 << 20, 2048, 1))
        for variant, index, alignment in [('mm_q4_k', 0, 4), ('mv_q4_k', 0, 4), ('mv_q4_k', 1, 2),
                                          ('mm_q4_k', 2, 2), ('mv_q4_k', 3, 2), ('mm_q4_k', 4, 16),
                                          ('mv_q4_k', 4, 4), ('mm_q4_k', 6, 4)]:
            scalars, buffers, grid, local = k_fixture(variant)
            moved = replace(buffers[index], offset=buffers[index].offset+alignment//2,
                            root_bytes=buffers[index].root_bytes+alignment)
            with self.subTest(variant=variant, index=index):
                with self.assertRaises(ValueError):
                    validate_launch(variant, scalars, {**buffers, index: moved}, grid, local)
        # The matrix kernel reads scale bytes singly: an odd offset is fine there.
        scalars, buffers, grid, local = k_fixture('mm_q4_k')
        odd = replace(buffers[1], offset=buffers[1].offset+1, root_bytes=buffers[1].root_bytes+1)
        validate_launch('mm_q4_k', scalars, {**buffers, 1: odd}, grid, local)

    def test_output_may_not_alias_any_of_the_five_inputs(self):
        for variant in ('mm_q4_k', 'mv_q4_k'):
            scalars, buffers, grid, local = k_fixture(variant)
            for index in (0, 1, 2, 3, 4):
                with self.subTest(variant=variant, index=index):
                    shared = replace(buffers[6], root=buffers[index].root, root_bytes=1 << 20, offset=0,
                                     size=1 << 20)
                    source = replace(buffers[index], root_bytes=1 << 20)
                    with self.assertRaises(ValueError):
                        validate_launch(variant, scalars, {**buffers, index: source, 6: shared}, grid, local)

    def test_digest_is_the_gguf_block_order(self):
        k, n = 256, 2500  # more than one chunk
        blocks = k*n//256
        raw = bytes((7*i+3) % 251 for i in range(blocks*144))
        d = b''.join(raw[i*144:i*144+2] for i in range(blocks))
        dmin = b''.join(raw[i*144+2:i*144+4] for i in range(blocks))
        scales = b''.join(raw[i*144+4:i*144+16] for i in range(blocks))
        quants = b''.join(raw[i*144+16:i*144+144] for i in range(blocks))
        self.assertEqual(q4_k_digest(quants, scales, d, dmin, k, n), hashlib.sha256(raw).hexdigest())
        self.assertNotEqual(q4_k_digest(quants, scales, dmin, d, k, n), hashlib.sha256(raw).hexdigest())
        with self.assertRaises(ValueError):
            q4_k_digest(quants[:-1], scales, d, dmin, k, n)
        with self.assertRaises(ValueError):
            q4_k_digest(bytearray(quants), scales, d, dmin, k, n)


def q6_fixture(variant, k=512, n=72, m=3):
    blocks = n*k//256
    if variant == 'mm_q6_k':
        scalars, buffers, grid, local = k_fixture('mm_q4_k', k, n, m)
    else:
        scalars = {5: 32, 7: 16, 8: k, 9: n, 10: 1, 11: k, 12: 1, 13: n, 14: m, 15: 1, 16: 1}
        grid, local = ((n+7)//8*16, m*2, 1), (16, 2, 1)
        buffers = {4: BufferView(5, m*k*4+96, 64, m*k*4+32), 6: BufferView(7, m*n*4+80, 64, m*n*4+16)}
    for index, size in enumerate((128, 64, 16, 2)):
        buffers[index] = BufferView(index+1, blocks*size+64, 64, blocks*size)
    return scalars, buffers, grid, local


def q5_fixture(variant, k=512, n=72, m=3):
    """Q5_K: the Q4_K arguments one place later, after a fifth weight buffer."""
    base, buffers4, grid, local = k_fixture(variant.replace('q5', 'q4'), k, n, m)
    scalars = {index+1: value for index, value in base.items()}
    if variant == 'mv_q5_k':
        row = k//256*176
        scalars.update({11: row, 12: row*n, 13: row*n})
    blocks = n*k//256
    buffers = {5: replace(buffers4[4], root=6), 7: replace(buffers4[6], root=8)}
    for index, size in enumerate((128, 32, 12, 2, 2)):
        buffers[index] = BufferView(index+1, blocks*size+64, 64, blocks*size)
    return scalars, buffers, grid, local


class Q5KLaunchTests(unittest.TestCase):
    def test_real_layouts(self):
        for variant, k, n, m in [('mm_q5_k', 5120, 17408, 32), ('mv_q5_k', 5120, 17408, 1),
                                  ('mv_q5_k', 17408, 5120, 1), ('mm_q5_k', 256, 73, 67), ('mv_q5_k', 256, 4, 3)]:
            with self.subTest(variant=variant, shape=(k, n, m)):
                launch = validate_launch(variant, *q5_fixture(variant, k, n, m))
                self.assertEqual((launch.k, launch.n, launch.m), (k, n, m))
                self.assertEqual([end-start for _, start, end in launch.reads],
                                 [k*n//2, k*n//8, k*n//256*12, k*n//128, k*n//128, m*k*4])

    def test_every_argument_and_every_other_kernels_layout(self):
        for variant in ('mm_q5_k', 'mv_q5_k'):
            scalars, buffers, grid, local = q5_fixture(variant)
            for index in range(9, 22):
                with self.subTest(variant=variant, index=index):
                    with self.assertRaises(ValueError):
                        validate_launch(variant, {**scalars, index: scalars[index]+1}, buffers, grid, local)
            for missing in (*scalars, *buffers):
                with self.subTest(variant=variant, missing=missing):
                    with self.assertRaises(ValueError):
                        validate_launch(variant, {i: v for i, v in scalars.items() if i != missing},
                                        {i: v for i, v in buffers.items() if i != missing}, grid, local)
            for other in ('mm', 'mv', 'mm_q4_k', 'mv_q4_k', 'mm_q6_k', 'mv_q6_k'):
                with self.assertRaises(ValueError):
                    validate_launch(other, scalars, buffers, grid, local)
            with self.assertRaises(ValueError):
                validate_launch(variant, *k_fixture(variant.replace('q5', 'q4')))
        with self.assertRaises(ValueError):  # the Q4_K row stride is not the Q5_K one
            scalars, buffers, grid, local = q5_fixture('mv_q5_k')
            validate_launch('mv_q5_k', {**scalars, 11: 512//256*144}, buffers, grid, local)
        with self.assertRaises(ValueError):
            validate_launch('mv_q5_k', *q5_fixture('mv_q5_k', 256, 6, 1))

    def test_alignment_and_aliasing(self):
        for variant in ('mm_q5_k', 'mv_q5_k'):
            scalars, buffers, grid, local = q5_fixture(variant)
            odd = replace(buffers[1], offset=buffers[1].offset+1, root_bytes=buffers[1].root_bytes+1)
            validate_launch(variant, scalars, {**buffers, 1: odd}, grid, local)
            for index in (0, 3, 4):
                moved = replace(buffers[index], offset=buffers[index].offset+1,
                                root_bytes=buffers[index].root_bytes+1)
                with self.assertRaises(ValueError):
                    validate_launch(variant, scalars, {**buffers, index: moved}, grid, local)
            for index in (0, 1, 2, 3, 4, 5):
                shared = replace(buffers[7], root=buffers[index].root, root_bytes=1 << 20, offset=0, size=1 << 20)
                source = replace(buffers[index], root_bytes=1 << 20)
                with self.assertRaises(ValueError):
                    validate_launch(variant, scalars, {**buffers, index: source, 7: shared}, grid, local)

    def test_digest_is_the_gguf_block_order(self):
        k, n = 256, 300
        blocks = k*n//256
        raw = bytes((13*i+7) % 249 for i in range(blocks*176))
        part = lambda start, size: b''.join(raw[i*176+start:i*176+start+size] for i in range(blocks))
        d, dmin, scales, high, quants = part(0, 2), part(2, 2), part(4, 12), part(16, 32), part(48, 128)
        self.assertEqual(q5_k_digest(quants, high, scales, d, dmin, k, n), hashlib.sha256(raw).hexdigest())
        self.assertNotEqual(q5_k_digest(quants, high, scales, dmin, d, k, n), hashlib.sha256(raw).hexdigest())
        with self.assertRaises(ValueError):
            q5_k_digest(quants, high[:-1], scales, d, dmin, k, n)


class Q6KLaunchTests(unittest.TestCase):
    def test_real_layouts_including_an_output_layer(self):
        for variant, k, n, m in [('mm_q6_k', 2048, 11008, 32), ('mv_q6_k', 11008, 2048, 1),
                                  ('mv_q6_k', 2048, 151936, 1), ('mv_q6_k', 256, 7, 3), ('mm_q6_k', 256, 73, 67)]:
            with self.subTest(variant=variant, shape=(k, n, m)):
                launch = validate_launch(variant, *q6_fixture(variant, k, n, m))
                self.assertEqual((launch.k, launch.n, launch.m), (k, n, m))
                self.assertEqual([end-start for _, start, end in launch.reads],
                                 [k*n//2, k*n//4, k*n//16, k*n//128, m*k*4])
                self.assertEqual(launch.write[2]-launch.write[1], m*n*4)

    def test_every_argument_of_the_vector_kernel_is_checked(self):
        scalars, buffers, grid, local = q6_fixture('mv_q6_k')
        for index in range(8, 17):
            with self.subTest(index=index):
                with self.assertRaises(ValueError):
                    validate_launch('mv_q6_k', {**scalars, index: scalars[index]+1}, buffers, grid, local)
        for missing in (*scalars, *buffers):
            with self.subTest(missing=missing):
                with self.assertRaises(ValueError):
                    validate_launch('mv_q6_k', {i: v for i, v in scalars.items() if i != missing},
                                    {i: v for i, v in buffers.items() if i != missing}, grid, local)
        for other in ('mv', 'mv_q4_k', 'mm_q6_k'):
            with self.assertRaises(ValueError):
                validate_launch(other, scalars, buffers, grid, local)
        with self.assertRaises(ValueError):
            validate_launch('mv_q6_k', *k_fixture('mv_q4_k'))
        with self.assertRaises(ValueError):
            validate_launch('mv_q6_k', scalars, buffers, grid, (16, 1, 1))
        with self.assertRaises(ValueError):
            validate_launch('mv_q6_k', {**scalars, 5: 34}, buffers, grid, local)
        with self.assertRaises(ValueError):  # block count is a signed int product
            validate_launch('mv_q6_k', *q6_fixture('mv_q6_k', 1 << 20, 1 << 20, 1))

    def test_alignment_and_aliasing(self):
        for variant in ('mm_q6_k', 'mv_q6_k'):
            scalars, buffers, grid, local = q6_fixture(variant)
            for index in (0, 1, 2):  # byte reads: any offset
                odd = replace(buffers[index], offset=buffers[index].offset+1,
                              root_bytes=buffers[index].root_bytes+1)
                validate_launch(variant, scalars, {**buffers, index: odd}, grid, local)
            odd = replace(buffers[3], offset=buffers[3].offset+1, root_bytes=buffers[3].root_bytes+1)
            with self.assertRaises(ValueError):
                validate_launch(variant, scalars, {**buffers, 3: odd}, grid, local)
            for index in (0, 1, 2, 3, 4):
                with self.subTest(variant=variant, index=index):
                    shared = replace(buffers[6], root=buffers[index].root, root_bytes=1 << 20, offset=0,
                                     size=1 << 20)
                    source = replace(buffers[index], root_bytes=1 << 20)
                    with self.assertRaises(ValueError):
                        validate_launch(variant, scalars, {**buffers, index: source, 6: shared}, grid, local)

    def test_digest_is_the_gguf_block_order(self):
        k, n = 256, 300
        blocks = k*n//256
        raw = bytes((11*i+5) % 253 for i in range(blocks*210))
        part = lambda start, size: b''.join(raw[i*210+start:i*210+start+size] for i in range(blocks))
        low, high, scales, d = part(0, 128), part(128, 64), part(192, 16), part(208, 2)
        self.assertEqual(q6_k_digest(low, high, scales, d, k, n), hashlib.sha256(raw).hexdigest())
        self.assertNotEqual(q6_k_digest(low, scales*4, high[:len(scales)], d, k, n), hashlib.sha256(raw).hexdigest())
        with self.assertRaises(ValueError):
            q6_k_digest(low, high, scales, d[:-2], k, n)


class LaunchTests(unittest.TestCase):
    def test_real_kernel_layouts_and_mm_tail_tiles(self):
        for variant, k, n, m in [('mm', 896, 896, 32), ('mv', 896, 151936, 1),
                                  ('mm', 96, 73, 67), ('mv', 32, 8, 3)]:
            with self.subTest(variant=variant, shape=(k, n, m)):
                launch = validate_launch(variant, *fixture(variant, k, n, m))
                self.assertEqual((launch.k, launch.n, launch.m), (k, n, m))
                self.assertEqual(launch.write[2]-launch.write[1], m*n*4)

    def test_every_shape_stride_and_broadcast_argument_is_checked(self):
        for variant in ('mm', 'mv'):
            scalars, buffers, grid, local = fixture(variant)
            for index in range(6, 19):
                with self.subTest(variant=variant, index=index):
                    with self.assertRaises(ValueError):
                        validate_launch(variant, {**scalars, index: scalars[index]+1}, buffers, grid, local)

    def test_missing_arguments_abi_ranges_and_offsets(self):
        for variant in ('mm', 'mv'):
            scalars, buffers, grid, local = fixture(variant)
            for index, value in [(3, -1), (5, 1 << 64), (6, 1 << 31), (6, True),
                                 (3, 3), (5, 3), (6, 0)]:
                with self.assertRaises(ValueError):
                    validate_launch(variant, {**scalars, index: value}, buffers, grid, local)
            with self.assertRaises(ValueError):
                validate_launch(variant, {k: v for k, v in scalars.items() if k != 18}, buffers, grid, local)
            with self.assertRaises(ValueError):
                validate_launch(variant, scalars, buffers, grid, local, (1, 0, 0))
            for bad_grid, bad_local in [((grid[0]-local[0], *grid[1:]), local),
                                         ((grid[0]+local[0], *grid[1:]), local),
                                         ((grid[0], grid[1], 2), local),
                                         (grid, (local[0]//2, *local[1:])),
                                         (grid, None), (grid[:2], local)]:
                with self.assertRaises(ValueError):
                    validate_launch(variant, scalars, buffers, bad_grid, bad_local)

    def test_buffer_ranges_aliases_and_subbuffer_alignment(self):
        for variant in ('mm', 'mv'):
            scalars, buffers, grid, local = fixture(variant)
            for index in buffers:
                with self.subTest(variant=variant, index=index):
                    bad = {**buffers, index: replace(buffers[index], size=buffers[index].size-1)}
                    with self.assertRaises(ValueError):
                        validate_launch(variant, scalars, bad, grid, local)
            for index in (0, 1, 2):
                # Distinct cl_mem/sub-buffer identities can share backing storage.
                bad = {**buffers, index: BufferView(77, 1 << 20, 64, buffers[index].size),
                       4: BufferView(77, 1 << 20, 64, buffers[4].size)}
                with self.assertRaises(ValueError):
                    validate_launch(variant, scalars, bad, grid, local)
            bad = {**buffers, 2: replace(buffers[2], offset=65, root_bytes=1 << 20)}
            with self.assertRaises(ValueError):
                validate_launch(variant, scalars, bad, grid, local)
            # Disjoint ranges in the same allocation are legitimate.
            good = {**buffers, 2: BufferView(77, 1 << 20, 64, buffers[2].size),
                    4: BufferView(77, 1 << 20, 16384, buffers[4].size)}
            validate_launch(variant, scalars, good, grid, local)
            good[4] = replace(good[4], root_bytes=(1 << 20)+1)
            with self.assertRaises(ValueError):
                validate_launch(variant, scalars, good, grid, local)

    def test_integer_overflow_and_unguarded_vector_tail(self):
        for variant, k, n, m in [('mv', 32, 9, 1), ('mm', 1 << 20, 2048, 1),
                                  ('mv', 65536, 65536, 1), ('mm', 32, 8, 1 << 30)]:
            with self.assertRaises(ValueError):
                validate_launch(variant, *fixture(variant, k, n, m))


def public_fixture(blocks=8193):
    # Independent block-major construction, including all signed code byte
    # values and half encodings without converting through float.
    packed = b''.join(struct.pack('<H', i % 65536) + bytes((i+j) % 256 for j in range(32))
                      for i in range(blocks))
    quants = b''.join(packed[i+2:i+34] for i in range(0, len(packed), 34))
    scales = b''.join(packed[i:i+2] for i in range(0, len(packed), 34))
    entry = {'name': 'public.weight', 'type': 8, 'ne': [32, blocks, 1, 1],
             'nbytes': len(packed), 'sha256': hashlib.sha256(packed).hexdigest()}
    document = {'schema': 'shield-public-tensors/1', 'modelSha256': 'a'*64,
                'calibrationSha256': 'b'*64, 'tensors': [entry]}
    return document, quants, scales, packed


def catalog(document):
    policy = json.dumps(document).encode()
    return PublicQ8Catalog(policy, hashlib.sha256(policy).hexdigest(), 'a'*64, 'b'*64)


class PublicWeightTests(unittest.TestCase):
    def test_exact_interleaved_digest_across_chunks_and_all_half_bits(self):
        document, q, d, packed = public_fixture(65536)
        self.assertEqual(q8_digest(q, d, 32, 65536), hashlib.sha256(packed).hexdigest())
        tensor = catalog(document).authenticate('public.weight', q, d, 32, 65536)
        self.assertIs(tensor.quants, q)
        self.assertIs(tensor.scales, d)

    def test_private_substitution_shape_and_later_mutation_are_rejected(self):
        document, q, d, _ = public_fixture()
        approved = catalog(document)
        approved.authenticate('public.weight', q, d, 32, 8193)
        for name, codes, scales, k, n in [
                ('private.weight', q, d, 32, 8193),
                ('public.weight', bytes([q[0] ^ 1])+q[1:], d, 32, 8193),
                ('public.weight', q, d[:-1]+bytes([d[-1] ^ 1]), 32, 8193),
                ('public.weight', q[:-1], d, 32, 8193),
                ('public.weight', q, d[:-1], 32, 8193),
                ('public.weight', q, d, 96, 2731),
                ('public.weight', bytearray(q), d, 32, 8193)]:
            with self.assertRaises(ValueError):
                approved.authenticate(name, codes, scales, k, n)

    def test_policy_is_bound_to_service_digests(self):
        document, _, _, _ = public_fixture(8)
        policy = json.dumps(document).encode()
        digest = hashlib.sha256(policy).hexdigest()
        for args in [(policy+b' ', digest, 'a'*64, 'b'*64),
                     (policy, digest, 'c'*64, 'b'*64),
                     (policy, digest, 'a'*64, 'c'*64)]:
            with self.assertRaises(ValueError):
                PublicQ8Catalog(*args)
        for update in [{'ne': [31, 8, 1, 1]}, {'nbytes': 17}, {'sha256': 'z'*64}]:
            bad = {**document, 'tensors': [{**document['tensors'][0], **update}]}
            with self.assertRaises(ValueError):
                catalog(bad)
        with self.assertRaises(ValueError):
            catalog({**document, 'tensors': document['tensors']*2})
        duplicated = policy.replace(b'"schema":', b'"schema":"wrong","schema":')
        with self.assertRaises(ValueError):
            PublicQ8Catalog(duplicated, hashlib.sha256(duplicated).hexdigest(), 'a'*64, 'b'*64)


if __name__ == '__main__':
    unittest.main()
