"""Exercise real child compilers under inherited service resource limits."""
import json
from pathlib import Path
import resource
import subprocess
import sys
import unittest

ROOT = Path(__file__).resolve().parent


class CompilerLimitTests(unittest.TestCase):
    def compile(self, helper, source, options, file_limit):
        def restrict():
            resource.setrlimit(resource.RLIMIT_FSIZE, (file_limit, file_limit))
        result = subprocess.run([sys.executable, '-I', str(ROOT/helper)],
                                input=json.dumps({'source': source, 'options': options}),
                                text=True, capture_output=True, preexec_fn=restrict, timeout=30,
                                check=True)
        return json.loads(result.stdout)

    def test_native_file_limit_keeps_integer_and_q8_analysis_working(self):
        plan = self.compile('kernel_compiler.py', (ROOT/'gemm_i8.cl').read_text(), '', 8<<20)
        self.assertEqual(len(plan), 1)
        self.assertEqual(next(iter(plan.values()))['operation'], 'gemm-i8-i8-i64')
        for name in ('mul_mm_q8_0_f32_l4_lm', 'mul_mv_q8_0_f32_flat'):
            with self.subTest(kernel=name):
                plan = self.compile('q8_capture_compiler.py',
                                    (ROOT/'q8-reference'/f'{name}.cl').read_text(), '-cl-std=CL3.0', 8<<20)
                self.assertFalse(plan['kernel_'+name]['dispatchable'])

    def test_unusable_parent_limit_declines_without_granting_dispatch(self):
        for helper, path, options in [('kernel_compiler.py', ROOT/'gemm_i8.cl', ''),
                ('q8_capture_compiler.py', ROOT/'q8-reference/mul_mm_q8_0_f32_l4_lm.cl', '-cl-std=CL3.0')]:
            self.assertEqual(self.compile(helper, path.read_text(), options, 1024), {})

    def test_child_never_raises_parent_soft_or_hard_limits(self):
        code = '''import json, resource
from kernel_compiler import limits
resource.setrlimit(resource.RLIMIT_FSIZE, (4<<20,8<<20))
limits()
print(json.dumps(resource.getrlimit(resource.RLIMIT_FSIZE)))
'''
        result = subprocess.run([sys.executable, '-c', code], cwd=ROOT,
                                text=True, capture_output=True, check=True)
        self.assertEqual(json.loads(result.stdout), [4<<20, 4<<20])


if __name__ == '__main__':
    unittest.main()
