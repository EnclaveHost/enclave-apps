import unittest
from pathlib import Path

from q8_kernel_compiler import analyze, FAST, source_allowed

ROOT = Path(__file__).with_name('q8-reference')
MM = (ROOT / 'mul_mm_q8_0_f32_l4_lm.cl').read_text()
MV = (ROOT / 'mul_mv_q8_0_f32_flat.cl').read_text()
MM_NAME = 'kernel_mul_mm_q8_0_f32_l4_lm'
MV_NAME = 'kernel_mul_mv_q8_0_f32_flat'


class Q8CompilerTests(unittest.TestCase):
    def test_real_sources_and_both_arithmetic_profiles(self):
        for source, name, variant in [(MM, MM_NAME, 'mm'), (MV, MV_NAME, 'mv')]:
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                with self.subTest(variant=variant, options=options):
                    plan = analyze(source, options)[name]
                    self.assertEqual(plan['variant'], variant)
                    self.assertFalse(plan['dispatchable'])
                    self.assertIn('authenticated-public-q8-tensor', plan['requires'])

    def test_symbol_spelling_is_not_semantics(self):
        for source, name in [(MM, MM_NAME), (MV, MV_NAME)]:
            renamed = source.replace(name, 'ordinary_kernel').replace('src0_q', 'weight_codes')
            renamed = '// unicode comment: π\n' + renamed.replace('offset1', 'input_offset')
            self.assertIn('ordinary_kernel', analyze(renamed))

    def test_swizzles_and_subgroup_attribute_are_semantic(self):
        for source, old, new in [(MM, 'v.s0', 'v.s1'), (MV, 'yl.s0', 'yl.s1'),
                                  (MV, 'tot.s0', 'tot.s3'),
                                  (MV, 'intel_reqd_sub_group_size(16)', 'intel_reqd_sub_group_size(32)')]:
            with self.subTest(change=(old, new)):
                self.assertIn(old, source)
                self.assertEqual(analyze(source.replace(old, new)), {})

    def test_arithmetic_index_bounds_and_outputs_are_preserved(self):
        cases = [(MM, 'block < ne00', 'block <= ne00'),
                 (MM, 'mad(cache_a[cr], cache_b[cc], sums[sums_idx])',
                  'mad(cache_a[cr], cache_b[cc], sums[sums_idx]) + 1.0f'),
                 (MM, 'src0_d[ib]', 'src0_d[ib+1]'),
                 (MM, '(dc + cc) * stride_d + dr + cr', '(dc + cc) * stride_d + dr'),
                 (MV, 'sumq += qv.s0*yl.s0', 'sumq -= qv.s0*yl.s0'),
                 (MV, 'offset_src0/34', 'offset_src0/32'),
                 (MV, 'get_group_id(1)', 'get_group_id(0)'),
                 (MV, 'global char * src0_q', 'global uchar * src0_q')]
        for source, old, new in cases:
            with self.subTest(change=(old, new)):
                self.assertIn(old, source)
                self.assertEqual(analyze(source.replace(old, new)), {})

    def test_redeclared_builtins_helpers_and_global_state_are_refused(self):
        for prefix in ['size_t get_group_id(uint);\n',
                       'size_t get_group_id(uint) __asm__("get_global_id");\n',
                       'float helper(float x) {return x;}\n',
                       '__constant int hidden = 1;\n']:
            with self.subTest(prefix=prefix):
                self.assertEqual(analyze(prefix + MM), {})

    def test_extra_stores_and_unknown_attributes_are_not_erased(self):
        self.assertEqual(analyze(MM.replace('sums[i] = 0.0f;', 'sums[i] = 0.0f; dst[0] = 1.0f;')), {})
        self.assertEqual(analyze(MM.replace('kernel void', '__attribute__((reqd_work_group_size(64,1,1))) kernel void')), {})

    def test_preprocessing_and_build_options_cannot_hide_semantics(self):
        for prefix in ['#pragma OPENCL FP_CONTRACT OFF\n', '#include "other.cl"\n',
                       '#line 1 "header.h"\n', '_Pragma("OPENCL FP_CONTRACT OFF")\n',
                       '#define get_group_id get_global_id\n']:
            with self.subTest(prefix=prefix):
                self.assertEqual(analyze(prefix + MM), {})
        for options in ['-cl-std=CL1.2', '-cl-std=CL3.0 -Dchar=uchar',
                        '-cl-std=CL3.0 -cl-denorms-are-zero', '-cl-std=CL3.0 -cl-std=CL3.0']:
            self.assertEqual(analyze(MM, options), {})

    def test_directive_filter_uses_c_comment_boundaries(self):
        for prefix in ['/* comment */ #pragma OPENCL FP_CONTRACT OFF\n',
                       '/* multi\nline */ #include "other.cl"\n',
                       '%:pragma OPENCL FP_CONTRACT OFF\n',
                       '??=pragma OPENCL FP_CONTRACT OFF\n',
                       '#define JOIN(a,b) a ## b\nJOIN(_Pr,agma)("OPENCL FP_CONTRACT OFF")\n',
                       '#define STR(x) #x\n', '#define hidden 1\n',
                       '#pragma OPENCL EXTENSION cl_khr_fp64 : enable\n']:
            with self.subTest(prefix=prefix):
                self.assertFalse(source_allowed(prefix + MM))
                self.assertEqual(analyze(prefix + MM), {})
        decorated = MM.replace('#pragma', '/* harmless */ #pragma')
        self.assertTrue(source_allowed(decorated))
        self.assertIn(MM_NAME, analyze(decorated))
        self.assertTrue(source_allowed('// _Pragma \\ %: ??=\n' + MM))


if __name__ == '__main__':
    unittest.main()
