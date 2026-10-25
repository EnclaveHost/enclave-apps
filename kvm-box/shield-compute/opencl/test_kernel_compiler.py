import unittest
from pathlib import Path
from kernel_compiler import compile_source

SOURCE = Path(__file__).with_name('gemm_i8.cl').read_text()

class CompilerTests(unittest.TestCase):
    def test_recognizes_typed_kernel(self):
        self.assertEqual(compile_source(SOURCE)['gemm']['operation'], 'gemm-i8-i8-i64')

    def test_names_comments_and_declaration_grouping_do_not_define_semantics(self):
        renamed = SOURCE.replace('gemm', 'ordinary_matmul').replace('row', 'i').replace('col', 'j')
        renamed = renamed.replace(', j=get_global_id(1)', '; size_t j=get_global_id(1)')
        self.assertIn('ordinary_matmul', compile_source('// comment\n' + renamed))

    def test_semantic_changes_stay_on_cpu(self):
        for old, new in [('k<K', 'k<=K'), ('long sum=0', 'long sum=1'),
                         ('b[k*N+col]', 'b[k+col]'), ('sum+=', 'sum-='),
                         ('(long)a', '(uchar)a'), ('char *a', 'uchar *a'),
                         ('c[row*N+col]=sum', 'c[row*N+col]=sum+1'),
                         ('long sum=0', 'int sum=0'), ('get_global_id(1)', 'get_global_id(0)')]:
            with self.subTest(change=new):
                self.assertEqual(compile_source(SOURCE.replace(old, new)), {})

    def test_unknown_float_kernel_is_not_quantized(self):
        self.assertEqual(compile_source('__kernel void scale(__global float*x){x[get_global_id(0)] *= 1.1f;}'), {})

    def test_build_flags_cannot_change_recognized_semantics(self):
        self.assertEqual(compile_source(SOURCE, '-Dlong=int'), {})
        self.assertEqual(compile_source(SOURCE, '-cl-fast-relaxed-math'), {})

    def test_extra_store_and_helper_definition_are_not_erased(self):
        self.assertEqual(compile_source(SOURCE.replace('c[row*N+col]=sum;', 'c[row*N+col]=sum; c[0]=0;')), {})
        self.assertEqual(compile_source('long helper(){return 0;}\n'+SOURCE), {})

    def test_builtin_redeclaration_cannot_redirect_indexing(self):
        for declaration in ('size_t get_global_id(uint);',
                            'size_t get_global_id(uint) __asm__("get_local_id");'):
            with self.subTest(declaration=declaration):
                self.assertEqual(compile_source(declaration + '\n' + SOURCE), {})

if __name__ == '__main__': unittest.main()
