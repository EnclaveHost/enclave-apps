import hashlib
import json
import struct
import unittest
from dataclasses import replace

from q8_admission import BufferView, PublicQ8Catalog, q8_digest, validate_launch


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
