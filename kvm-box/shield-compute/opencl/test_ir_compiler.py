"""Structural IR regression tests; no service/GPU execution is claimed here."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
import struct
from kernel_compiler import compile_modules

REFERENCE = Path(os.environ.get('SHIELD_IR_REFERENCE', '/tests/gemm-reference.bc'))
COMPILER = Path(__file__).with_name('ir-compiler')


class IRCompilerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.reference = subprocess.check_output(
            ['llvm-dis-18', str(REFERENCE), '-o', '-'], text=True)

    def compile(self, text):
        with tempfile.TemporaryDirectory() as directory:
            candidate = Path(directory) / 'candidate.ll'
            candidate.write_text(text)
            result = subprocess.run([str(COMPILER), str(REFERENCE), str(candidate)],
                                    capture_output=True, text=True, timeout=15, check=True)
            return json.loads(result.stdout)

    def mutate(self, old, new):
        self.assertIn(old, self.reference)
        return self.reference.replace(old, new)

    def test_original_and_kernel_renaming(self):
        self.assertIn('gemm', self.compile(self.reference))
        self.assertIn('user_matmul', self.compile(self.mutate('@gemm(', '@user_matmul(')))

    def test_changed_math_bounds_and_signedness(self):
        for old, new in [
            ('mul nsw i64', 'sub nsw i64'),
            ('add nsw i64', 'sub nsw i64'),
            ('sext i8', 'zext i8'),
            ('icmp ult', 'icmp ule'),
            ('i32 1) #1', 'i32 0) #1'),
            ('store i64 %.023,', 'store i64 17,'),
            ('load i8,', 'load volatile i8,'),
        ]:
            with self.subTest(change=new):
                self.assertEqual(self.compile(self.mutate(old, new)), {})

    def test_extra_side_effect(self):
        candidate = self.mutate('  ret void', '  store i64 0, ptr addrspace(1) %2, align 8\n  ret void')
        self.assertEqual(self.compile(candidate), {})

    def test_builtin_definition_is_not_an_import(self):
        candidate = self.mutate(
            'declare spir_func i64 @_Z12get_group_idj(i32) #1',
            'define spir_func i64 @_Z12get_group_idj(i32 %x) #1 { ret i64 0 }')
        self.assertEqual(self.compile(candidate), {})

    def test_unknown_builtin_alias_and_global_state(self):
        self.assertEqual(self.compile(self.mutate('_Z12get_group_idj', '_Z13get_global_idj')), {})
        self.assertEqual(self.compile(self.reference + '\n@state = addrspace(1) global i32 0\n'), {})
        self.assertEqual(self.compile(self.reference + '\n@alias = alias void (ptr addrspace(1), ptr addrspace(1), ptr addrspace(1), i32, i32, i32), ptr @gemm\n'), {})

    def test_unknown_execution_metadata(self):
        candidate = self.mutate('!spirv.ParameterDecorations !9 {',
                                '!spirv.ParameterDecorations !9 !reqd_work_group_size !10 {')
        self.assertEqual(self.compile(candidate + '\n!10 = !{i32 1, i32 1, i32 1}\n'), {})

    def test_unrelated_function_does_not_authorize_it(self):
        candidate = self.reference + '\ndefine spir_kernel void @other(ptr addrspace(1) %p) { store i64 7, ptr addrspace(1) %p\n ret void }\n'
        self.assertEqual(set(self.compile(candidate)), {'gemm'})

    def test_real_chipstar_wrapper(self):
        path = Path('/tests/cuda-gemm-reverse.bc')
        if not path.exists():
            self.skipTest('separate observed chipStar module not supplied')
        text = subprocess.check_output(['llvm-dis-18', str(path), '-o', '-'], text=True)
        self.assertIn('gemm', self.compile(text))

    def test_spirv_specialization_and_malformed_input_are_rejected(self):
        spirv = REFERENCE.with_suffix('.spv').read_bytes()
        self.assertIn('gemm', compile_modules([spirv.hex()]))
        words = list(struct.unpack('<' + 'I' * (len(spirv) // 4), spirv))
        offset = 5
        while offset < len(words):
            length, opcode = words[offset] >> 16, words[offset] & 65535
            if opcode == 43:  # OpConstant -> OpSpecConstant, same literal layout
                words[offset] = (length << 16) | 50
                break
            offset += length
        else:
            self.fail('fixture contains no ordinary constant')
        specialized = struct.pack('<' + 'I' * len(words), *words)
        self.assertEqual(compile_modules([specialized.hex()]), {})
        self.assertEqual(compile_modules([(b'BAD!' + spirv[4:]).hex()]), {})
        self.assertEqual(compile_modules([spirv[:-1].hex()]), {})


if __name__ == '__main__':
    unittest.main()
