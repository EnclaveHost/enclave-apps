import unittest
from pathlib import Path

from q8_kernel_compiler import analyze, FAST, source_allowed

ROOT = Path(__file__).with_name('q8-reference')
MM = (ROOT / 'mul_mm_q8_0_f32_l4_lm.cl').read_text()
MV = (ROOT / 'mul_mv_q8_0_f32_flat.cl').read_text()
MM_NAME = 'kernel_mul_mm_q8_0_f32_l4_lm'
MV_NAME = 'kernel_mul_mv_q8_0_f32_flat'


MM_K = (ROOT / 'mul_mm_q4_k_f32_l4_lm.cl').read_text()
MV_K = (ROOT / 'mul_mv_q4_k_f32_flat.cl').read_text()


MM_6 = (ROOT / 'mul_mm_q6_k_f32_l4_lm.cl').read_text()
MV_6 = (ROOT / 'mul_mv_q6_k_f32_flat.cl').read_text()


class Q8CompilerTests(unittest.TestCase):
    def test_the_iq4_xs_kernel_added_to_the_backend(self):
        source = (ROOT / 'mul_mat_iq4_xs_f32_raw.cl').read_text()
        for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
            plan = analyze(source, options)['kernel_mul_mat_iq4_xs_f32_raw']
            self.assertEqual((plan['variant'], plan['operation']), ('raw_iq4_xs', 'iq4_xs-f32-matmul'))
            self.assertEqual(plan['arguments'], {'weights': 0, 'weightsOffset': 1, 'input': 2, 'inputOffset': 3,
                                                 'output': 4, 'outputOffset': 5})
        # The table of values, the scale bits and the block size are semantics.
        for old, new in [('-22, -10, 1, 13', '-22, -10, 2, 13'), ('(2 * part)) & 3) << 4)) - 32', '(2 * part)) & 3) << 4)) - 31'),
                         ('(ulong)nb * 136', '(ulong)nb * 144'), ('y[i + 16]', 'y[i + 15]')]:
            with self.subTest(change=old):
                self.assertIn(old, source)
                self.assertEqual(analyze(source.replace(old, new, 1)), {})

    def test_the_q3_k_and_iq3_s_kernels_added_to_the_backend(self):
        cases = {
            'q3_k': [('(scale - 32)', None), ('(ulong)nb * 110', '(ulong)nb * 136'), ('? 0 : 4)', '? 0 : 3)'),
                     ('block + 108', 'block + 106'), ('<< (half_block * 4 + pair)', '<< (half_block * 4)')],
            'iq3_s': [('0x01010101, 0x01010103', '0x01010101, 0x01010105'), ('block[66 + part]', 'block[67 + part]'),
                      ('1 + 2 * ((block[106', '2 * ((block[106'), ('? -magnitude : magnitude', '? magnitude : -magnitude'),
                      ('0x0f0d0703, 0x0f0f0101,\n};', '0x0f0d0703, 0x0f0f0103,\n};')],
        }
        for weights, changes in cases.items():
            source = (ROOT / f'mul_mat_{weights}_f32_raw.cl').read_text()
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                plan = analyze(source, options, compare=True)[f'kernel_mul_mat_{weights}_f32_raw']
                self.assertEqual((plan['variant'], plan['operation']), ('raw_' + weights, weights + '-f32-matmul'))
            for old, new in changes:
                if new is None:
                    continue
                with self.subTest(weights=weights, change=old):
                    self.assertIn(old, source)
                    self.assertEqual(analyze(source.replace(old, new, 1)), {})

    def test_a_reference_itself_gets_the_plan_the_comparison_gives(self):
        from q8_kernel_compiler import KERNELS, REFERENCES
        self.assertEqual(set(KERNELS), set(REFERENCES))
        for variant, filename in REFERENCES.items():
            source = (ROOT / filename).read_text()
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                with self.subTest(variant=variant, options=options):
                    direct = analyze(source, options)
                    self.assertEqual(direct, analyze(source, options, compare=True))
                    self.assertEqual(list(direct), [KERNELS[variant]])
            # One more byte and the source is compared, not looked up.
            self.assertEqual(analyze(source + '\n'), analyze(source + '\n', compare=True))

    def test_iq4_nl_sources_with_their_table_and_nested_functions(self):
        for file, name, variant in [('mul_mm_iq4_nl_f32_l4_lm.cl', 'kernel_mul_mm_iq4_nl_f32_l4_lm', 'mm_iq4_nl'),
                                    ('mul_mv_iq4_nl_f32_flat.cl', 'kernel_mul_mv_iq4_nl_f32_flat', 'mv_iq4_nl')]:
            source = (ROOT / file).read_text()
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                plan = analyze(source, options)[name]
                self.assertEqual((plan['variant'], plan['operation']), (variant, 'iq4_nl-f32-matmul'))
            # One value of the table every kernel reads.
            self.assertIn('25.f', source)
            self.assertEqual(analyze(source.replace('25.f', '26.f', 1)), {})
            # File-scope storage a kernel could write is still refused.
            self.assertEqual(analyze(source.replace('constant float kvalues_iq4nl', 'float kvalues_iq4nl', 1)), {})
        source = (ROOT / 'mul_mv_iq4_nl_f32_flat.cl').read_text()
        self.assertEqual(analyze(source.replace('kvalues_iq4nl[qs[i] >> 4]', 'kvalues_iq4nl[qs[i] >> 3]', 1)), {})
        self.assertEqual(analyze(source.replace('d + ib + 5*nb', 'd + ib + 4*nb', 1)), {})

    def test_q5_k_sources(self):
        for file, name, variant in [('mul_mm_q5_k_f32_l4_lm.cl', 'kernel_mul_mm_q5_k_f32_l4_lm', 'mm_q5_k'),
                                    ('mul_mv_q5_k_f32_flat.cl', 'kernel_mul_mv_q5_K_f32_flat', 'mv_q5_k')]:
            source = (ROOT / file).read_text()
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                with self.subTest(variant=variant, options=options):
                    plan = analyze(source, options)[name]
                    self.assertEqual((plan['variant'], plan['operation']), (variant, 'q5_k-f32-matmul'))
                    self.assertEqual(plan['arguments']['input'], 5)
        source = (ROOT / 'mul_mv_q5_k_f32_flat.cl').read_text()
        self.assertEqual(analyze(source.replace('qh[2]&u1_lo?16.f:0.f', 'qh[3]&u1_lo?16.f:0.f', 1)), {})
        self.assertEqual(analyze(source.replace('qh += blk*32;', 'qh += blk*31;')), {})

    def test_q6_k_sources_and_the_function_the_vector_kernel_calls(self):
        for source, name, variant in [(MM_6, 'kernel_mul_mm_q6_k_f32_l4_lm', 'mm_q6_k'),
                                      (MV_6, 'kernel_mul_mv_q6_K_f32_flat', 'mv_q6_k')]:
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                with self.subTest(variant=variant, options=options):
                    plan = analyze(source, options)[name]
                    self.assertEqual((plan['variant'], plan['operation']), (variant, 'q6_k-f32-matmul'))
                    self.assertEqual(plan['arguments']['quantsHigh'], 1)
        # The vector kernel's arithmetic is in a function it calls: that body
        # is part of the comparison, whatever the function is called.
        self.assertIn('kernel_mul_mv_q6_K_f32_flat', analyze(MV_6.replace('block_q_6_K_dot_y_flat', 'helper')))
        for old, new in [('dot(y1, w1) * sc[2]', 'dot(y1, w1) * sc[3]'), ('- 32.f;', '- 31.f;'),
                         ('ib*64 + q_offset_h', 'ib*64 + q_offset_l')]:
            with self.subTest(change=old):
                self.assertIn(old, MV_6)
                self.assertEqual(analyze(MV_6.replace(old, new, 1)), {})
        # A second, different function of the same name cannot stand in.
        self.assertEqual(analyze(MV_6.replace('return dall * (', 'return 2 * dall * (')), {})
        self.assertEqual(analyze(MM_6.replace('& 3) << 4', '& 3) << 5', 1)), {})

    def test_q4_k_sources_are_recognised_as_their_own_operation(self):
        for source, name, variant in [(MM_K, 'kernel_mul_mm_q4_k_f32_l4_lm', 'mm_q4_k'),
                                      (MV_K, 'kernel_mul_mv_q4_K_f32_flat', 'mv_q4_k')]:
            for options in ['-cl-std=CL3.0', ' '.join(FAST)]:
                with self.subTest(variant=variant, options=options):
                    plan = analyze(source, options)[name]
                    self.assertEqual((plan['variant'], plan['operation']), (variant, 'q4_k-f32-matmul'))
                    self.assertFalse(plan['dispatchable'])
                    self.assertEqual(plan['arguments']['input'], 4)
        # Constants and partly initialised arrays are part of the comparison.
        self.assertEqual(analyze(MV_K.replace('kmask2 = 0x0f0f', 'kmask2 = 0x0f0e')), {})
        self.assertEqual(analyze(MV_K.replace('{0.f}', '{1.f}')), {})
        self.assertEqual(analyze(MM_K.replace('0x0F', '0x0E', 1)), {})

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
