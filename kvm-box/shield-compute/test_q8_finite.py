"""IEEE-754 boundary regression for the local Q8 transport validator."""
import math
import struct
import unittest

from q8_service import MAX_BYTES, finite_f32


class FiniteFloatTests(unittest.TestCase):
    def test_all_high_words_with_representative_low_mantissas(self):
        # Enumerate sign, every exponent and upper mantissa bits. Lower bits
        # cannot alter finiteness; include zero, smallest and maximal mantissas.
        for high in range(65536):
            for low in (0, 1, 65535):
                data = struct.pack('<I', (high << 16) | low)
                self.assertEqual(finite_f32(data), math.isfinite(struct.unpack('<f', data)[0]))

    def test_nonfinite_anywhere_in_maximum_frame_is_rejected(self):
        original = struct.pack('<f', .125) * (MAX_BYTES // 4)
        self.assertTrue(finite_f32(original))
        for offset in (0, 4, 124, MAX_BYTES // 2, MAX_BYTES - 4):
            for bits in (0x7f800000, 0xff800000, 0x7f800001, 0xffc00000):
                data = bytearray(original)
                struct.pack_into('<I', data, offset, bits)
                self.assertFalse(finite_f32(bytes(data)))

    def test_empty_partial_and_finite_extremes(self):
        self.assertTrue(finite_f32(b''))
        for count in (1, 2, 3, 5, 6, 7):
            self.assertFalse(finite_f32(bytes(count)))
        finite = (0, 0x80000000, 1, 0x80000001, 0x007fffff, 0x00800000, 0x7f7fffff, 0xff7fffff)
        self.assertTrue(finite_f32(struct.pack('<8I', *finite)))


if __name__ == '__main__': unittest.main()
