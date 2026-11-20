#!/usr/bin/env python3
"""The native launch admission (q8_admission.inc) agrees with q8_admission.py.

Builds the small driver with g++ and compares its answer with validate_launch
for valid launches of both kernels, every single-field change to them, boundary
values and seeded random launches. Needs g++; no OpenCL runtime or GPU.
"""
from pathlib import Path
import random
import subprocess
import tempfile
import unittest

from q8_admission import BufferView, validate_launch

HERE = Path(__file__).resolve().parent
I32 = (1 << 31) - 1
INTERESTING = (0, 1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33, 34, 63, 64, 65, 127, 128, 896, 4864,
               I32 - 128, I32 - 8, I32, I32 + 1, (1 << 32) - 1, 1 << 32, (1 << 63) - 1, (1 << 64) - 1)


KINDS = {'mm': 1, 'mv': 2, 'mm_q4_k': 3, 'mv_q4_k': 4, 'mm_q6_k': 5, 'mv_q6_k': 6, 'mm_q5_k': 7, 'mv_q5_k': 8,
         'raw_iq4_xs': 9, 'mm_iq4_nl': 10, 'mv_iq4_nl': 11}


def launch(variant, k, n, m, *, input_offset=0, output_offset=0, shared_root=False):
    """Scalars in the Q8 numbering; a Q4_K kernel's argument is two places later."""
    if variant == 'raw_iq4_xs':
        # Held as: 3 input offset, 4 weights offset, 5 output offset, 6 k, 7 n, 8 m.
        weights_offset = 2 * (input_offset // 4)
        sizes = [weights_offset + n*k//256*136, input_offset + m*k*4, output_offset + m*n*4]
        scalars = {3: input_offset, 4: weights_offset, 5: output_offset, 6: k, 7: n, 8: m}
        if shared_root:
            total, buffers, offset = sum((size + 63)//64*64 for size in sizes), [], 0
            for size in sizes:
                buffers.append([1000, total, offset, size])
                offset += (size + 63)//64*64
        else:
            buffers = [[1000 + i, size, 0, size] for i, size in enumerate(sizes)]
        return {'variant': variant, 'scalars': scalars, 'buffers': buffers, 'global': [n, m, 1],
                'local': [1, 1, 1], 'offset': [0, 0, 0]}
    k_quant, q6, q5 = variant.endswith('_k'), variant.endswith('_q6_k'), variant.endswith('_q5_k')
    scalars = {3: input_offset, 5: output_offset, 6: k, 7: n, 17: 1, 18: 1}
    nibbles = variant.endswith('_iq4_nl')
    if variant in ('mv_q6_k', 'mv_iq4_nl'):
        scalars = {3: input_offset, 5: output_offset, 6: k, 7: n, 8: 1, 9: k, 10: 1, 11: n, 12: m, 13: 1, 14: 1}
        grid, local = ([(n+7)//8*16, m*2, 1], [16, 2, 1]) if q6 else ([(n+7)//8*16, m, 1], [16, 1, 1])
    elif variant.startswith('mm'):
        scalars.update({8: 1, 9: m, 10: 1, 11: k, 12: k, 13: n, 14: k*n, 15: k*m, 16: n*m})
        grid, local = [(n+63)//64*128, (m+63)//64, 1], [128, 1, 1]
    else:
        # the Q6_K vector kernel is handled above
        row = k//256*176 if q5 else k//256*144 if k_quant else k//32*34
        scalars.update({8: row, 9: row*n, 10: row*n, 11: 1, 12: k*4, 13: m*k*4, 14: m*k*4, 15: n, 16: m})
        grid, local = ([(n+3)//4*16, m, 1], [16, 1, 1]) if k_quant else ([(n+7)//8*16, m*2, 1], [16, 2, 1])
    weights = ([n*k//2, n*k//8, n*k//256*12, n*k//256*2, n*k//256*2] if q5 else
               [n*k//2, n*k//4, n*k//16, n*k//256*2] if q6 else
               [n*k//2, n*k//256*12, n*k//256*2, n*k//256*2] if k_quant else
               [n*k//2, n*k//16] if nibbles else [n*k, n*k//16])
    sizes = weights + [input_offset + m*k*4, output_offset + m*n*4]
    if shared_root:
        total, buffers, offset = sum((size + 63)//64*64 for size in sizes), [], 0
        for size in sizes:
            buffers.append([1000, total, offset, size])
            offset += (size + 63)//64*64
    else:
        buffers = [[1000 + i, size, 0, size] for i, size in enumerate(sizes)]
    return {'variant': variant, 'scalars': scalars, 'buffers': buffers, 'global': grid, 'local': local,
            'offset': [0, 0, 0]}


def reference(case):
    if case['variant'] == 'raw_iq4_xs':
        try:
            held = case['scalars']
            scalars = {1: held[4], 3: held[3], 5: held[5], 6: held[6], 7: held[7], 8: held[8]}
            buffers = {index: BufferView(*case['buffers'][i]) for i, index in enumerate((0, 2, 4))}
            result = validate_launch('raw_iq4_xs', scalars, buffers, case['global'], case['local'], case['offset'])
        except (ValueError, TypeError, KeyError, IndexError):
            return None
        return (result.m, result.n, result.k, *(end - start for _, start, end in result.reads),
                result.write[2] - result.write[1])
    k_quant, q5 = case['variant'].endswith('_k'), case['variant'].endswith('_q5_k')
    shift = 3 if q5 else 2 if k_quant else 0
    indices = (0, 1, 2, 3, 4, 5, 7) if q5 else (0, 1, 2, 3, 4, 6) if k_quant else (0, 1, 2, 4)
    try:
        buffers = {index: BufferView(*case['buffers'][i]) for i, index in enumerate(indices)}
        scalars = {index + shift: value for index, value in case['scalars'].items()}
        result = validate_launch(case['variant'], scalars, buffers, case['global'],
                                 case['local'], case['offset'])
    except (ValueError, TypeError):
        return None
    return (result.m, result.n, result.k, *(end - start for _, start, end in result.reads),
            result.write[2] - result.write[1])


def encode(case):
    scalars = case['scalars']
    values = [KINDS[case['variant']], scalars[3], scalars.get(4, 0)] + [scalars.get(i, 0) for i in range(5, 19)]
    for buffer in case['buffers'] + [[0, 0, 0, 0]]*(7 - len(case['buffers'])):
        values += buffer
    return ' '.join(str(v) for v in values + case['global'] + case['local'] + case['offset'])


def mutations(base, rng):
    for index in base['scalars']:
        for value in INTERESTING + (base['scalars'][index] + 1, max(base['scalars'][index] - 1, 0)):
            case = {**base, 'scalars': {**base['scalars'], index: value}}
            yield case
    for i in range(len(base['buffers'])):
        for field in range(4):
            for value in INTERESTING + (base['buffers'][i][field] + 1, max(base['buffers'][i][field] - 1, 0)):
                buffers = [list(b) for b in base['buffers']]
                buffers[i][field] = value
                yield {**base, 'buffers': buffers}
    for key in ('global', 'local', 'offset'):
        for i in range(3):
            for value in INTERESTING[:20] + (base[key][i] + 1, max(base[key][i] - 1, 0)):
                values = list(base[key])
                values[i] = value
                yield {**base, key: values}
    for _ in range(200):
        case = {**base, 'scalars': dict(base['scalars']), 'buffers': [list(b) for b in base['buffers']]}
        for _ in range(rng.randint(1, 3)):
            if rng.random() < 0.5:
                case['scalars'][rng.choice(list(case['scalars']))] = rng.choice(INTERESTING)
            else:
                case['buffers'][rng.randrange(len(case['buffers']))][rng.randrange(4)] = rng.choice(INTERESTING)
        yield case


class NativeAdmission(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.driver = str(Path(cls.directory.name) / 'driver')
        subprocess.run(['g++', '-std=c++20', '-O1', '-Wall', '-Wextra', '-Werror',
                        str(HERE / 'test_q8_admission_native.cpp'), '-I', str(HERE), '-o', cls.driver], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def compare(self, cases):
        cases = list(cases)
        output = subprocess.run([self.driver], input='\n'.join(encode(c) for c in cases) + '\n',
                                capture_output=True, text=True, check=True).stdout.splitlines()
        self.assertEqual(len(output), len(cases))
        accepted = 0
        for case, line in zip(cases, output):
            expected = reference(case)
            actual = tuple(int(x) for x in line.split()[1:]) if line != '0' else None
            self.assertEqual(actual, expected, encode(case))
            accepted += expected is not None
        return accepted

    def test_valid_launches(self):
        cases = [launch(variant, k, n, m, input_offset=io, output_offset=oo, shared_root=shared)
                 for variant in ('mm', 'mv', 'mm_iq4_nl', 'mv_iq4_nl')
                 for k, n in ((896, 4864), (4864, 896), (32, 8), (896, 151936), (96, 12))
                 for m in (1, 2, 3, 7, 32) for io, oo in ((0, 0), (32, 32), (64, 16))
                 for shared in (False, True)]
        self.assertGreater(self.compare(cases), len(cases) // 2)
        cases = [launch(variant, k, n, m, input_offset=io, output_offset=oo, shared_root=shared)
                 for variant in ('mm_q4_k', 'mv_q4_k', 'mm_q6_k', 'mv_q6_k', 'mm_q5_k', 'mv_q5_k', 'raw_iq4_xs')
                 for k, n in ((2048, 11008), (11008, 2048), (256, 4), (2048, 151936), (288, 8), (256, 6))
                 for m in (1, 2, 3, 7, 32) for io, oo in ((0, 0), (32, 32), (64, 16))
                 for shared in (False, True)]
        self.assertGreater(self.compare(cases), len(cases) // 2)

    def test_kinds_do_not_accept_each_others_layouts(self):
        cases = []
        for variant in KINDS:
            base = launch(variant, 2048 if variant.endswith('_k') else 896, 4864, 3)
            for other in KINDS:
                if other != variant and len(launch(other, 2048, 8, 1)['buffers']) == len(base['buffers']) \
                        and 'mv_q6_k' not in (other, variant) and 'mv_iq4_nl' not in (other, variant):
                    cases.append({**base, 'variant': other})
        # Geometry cannot tell the two matrix kernels with four weight buffers
        # apart when the buffers are large enough for both: Q6_K buffers hold
        # a Q4_K tensor of the same shape. Which kernel it is comes from the
        # recognised source, never from the launch.
        accepted = [c for c in cases if reference(c) is not None]
        self.assertEqual(self.compare(cases), len(accepted))
        # The same holds for the two matrix kernels with two weight buffers:
        # Q8_0 buffers are large enough for an IQ4_NL tensor of the same shape.
        self.assertEqual(sorted((c['variant'], len(c['buffers'])) for c in accepted),
                         [('mm_iq4_nl', 4), ('mm_q4_k', 6)])

    def test_every_field_and_random_changes(self):
        rng = random.Random(20261119)
        cases = []
        for variant in ('mm_q4_k', 'mv_q4_k', 'mm_q6_k', 'mv_q6_k', 'mm_q5_k', 'mv_q5_k', 'raw_iq4_xs'):
            for shared in (False, True):
                cases += mutations(launch(variant, 2048, 4864, 3, input_offset=32, output_offset=32,
                                          shared_root=shared), rng)
        for variant in ('mm', 'mv', 'mm_iq4_nl', 'mv_iq4_nl'):
            for shared in (False, True):
                cases += mutations(launch(variant, 896, 4864, 3, input_offset=32, output_offset=32,
                                          shared_root=shared), rng)
        accepted = self.compare(cases)
        self.assertGreater(len(cases), 17000)
        # Most changes must be refused, and some harmless ones (larger buffers) accepted.
        self.assertGreater(accepted, 20)
        self.assertLess(accepted, len(cases) // 4)

    def test_aliasing_output(self):
        base = launch('mm', 896, 4864, 3, shared_root=True)
        overlapping = {**base, 'buffers': [list(b) for b in base['buffers']]}
        overlapping['buffers'][3] = list(overlapping['buffers'][2])
        overlapping['buffers'][3][3] = max(base['buffers'][2][3], base['buffers'][3][3])
        overlapping['buffers'][2][3] = overlapping['buffers'][3][3]
        self.assertEqual(self.compare([overlapping, base]), 1)
        for variant in ('mm_q4_k', 'mv_q4_k', 'mm_q6_k', 'mv_q6_k', 'mm_q5_k', 'mv_q5_k'):
            base = launch(variant, 2048, 4864, 3, shared_root=True)
            out = len(base['buffers']) - 1
            for weight in range(out):
                case = {**base, 'buffers': [list(b) for b in base['buffers']]}
                case['buffers'][out] = list(case['buffers'][weight])
                case['buffers'][out][3] = case['buffers'][weight][3] = max(base['buffers'][weight][3],
                                                                       base['buffers'][out][3])
                self.assertEqual(self.compare([case]), 0, (variant, weight))


if __name__ == '__main__':
    unittest.main()
