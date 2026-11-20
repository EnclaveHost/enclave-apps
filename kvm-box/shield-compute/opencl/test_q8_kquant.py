#!/usr/bin/env python3
"""The K-quant decoding used for local products (q8_kquant.inc) gives the
weights gguf-py's reference dequantisation gives, for random blocks.

Needs g++, numpy and gguf-py (GGUF_PY=/path/to/gguf-py if it is not installed).
"""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import numpy as np

if os.environ.get('GGUF_PY'):
    sys.path.insert(0, os.environ['GGUF_PY'])
from gguf import GGMLQuantizationType, quants

HERE = Path(__file__).resolve().parent
# Byte layout of one block: (field, size) in GGUF order, and the order of the
# flat buffers the OpenCL backend keeps.
LAYOUT = {
    12: ([('d', 2), ('dmin', 2), ('scales', 12), ('qs', 128)], ['qs', 'scales', 'd', 'dmin']),
    13: ([('d', 2), ('dmin', 2), ('scales', 12), ('qh', 32), ('qs', 128)], ['qs', 'qh', 'scales', 'd', 'dmin']),
    14: ([('ql', 128), ('qh', 64), ('scales', 16), ('d', 2)], ['ql', 'qh', 'scales', 'd']),
    20: ([('d', 2), ('qs', 16)], ['qs', 'd']),
    23: ([('d', 2), ('scales_h', 2), ('scales_l', 4), ('qs', 128)], None),  # kept whole
}


class KQuantDecode(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.driver = str(Path(cls.directory.name) / 'driver')
        subprocess.run(['g++', '-std=c++20', '-O1', '-Wall', '-Wextra', '-Werror',
                        str(HERE / 'test_q8_kquant.cpp'), '-I', str(HERE), '-o', cls.driver], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def test_decoded_weights_equal_the_reference(self):
        rng = np.random.default_rng(20261120)
        for kind, name in ((12, 'Q4_K'), (13, 'Q5_K'), (14, 'Q6_K'), (20, 'IQ4_NL'), (23, 'IQ4_XS')):
            fields, flat = LAYOUT[kind]
            size = sum(width for _, width in fields)
            blocks = 257
            raw = rng.integers(0, 256, size=(blocks, size), dtype=np.uint8)
            # Ordinary finite half scales, both signs.
            offset = 0
            parts = {}
            for field, width in fields:
                if field in ('d', 'dmin'):
                    halves = (rng.standard_normal(blocks) * 0.05).astype(np.float16)
                    raw[:, offset:offset+2] = halves.view(np.uint8).reshape(blocks, 2)
                parts[field] = raw[:, offset:offset+width].tobytes()
                offset += width
            expected = quants.dequantize(raw.reshape(-1), GGMLQuantizationType[name]).reshape(-1)
            payload = f'{kind} {blocks}\n'.encode() + (raw.tobytes() if flat is None else
                                                        b''.join(parts[field] for field in flat))
            output = subprocess.run([self.driver], input=payload, capture_output=True, check=True).stdout
            actual = np.frombuffer(output, dtype='<f4')
            with self.subTest(type=name):
                self.assertEqual(actual.shape, expected.shape)
                np.testing.assert_array_equal(actual, expected.astype(np.float32))


if __name__ == '__main__':
    unittest.main()
