"""Run production frontend code against a deterministic recycling driver.

Run in the OpenCL builder with this directory at /src. Builds only in a private
temporary directory, leaving the normal runtime and the live VM untouched.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parent


class ProgramLifetimeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix='shield-lifetime-')
        cls.work = Path(cls.temp.name)
        for name in ('Makefile', 'opencl.map', 'shield_opencl.cpp'):
            shutil.copy2(ROOT / name, cls.work / name)
        cls.helper = cls.work / 'observe.py'
        cls.helper.write_text('''import json, os, sys, time
from pathlib import Path
with open(os.environ['SHIELD_LIFETIME_LOG'], 'a') as log:
    log.write(json.dumps(json.load(sys.stdin)) + '\\n')
if barrier := os.environ.get('SHIELD_LIFETIME_BARRIER'):
    Path(barrier + '.ready').touch()
    deadline = time.monotonic() + 5
    while not Path(barrier + '.continue').exists():
        assert time.monotonic() < deadline
        time.sleep(.001)
plans = {
    'integer': {'operation': 'gemm-i8-i8-i64'},
    'integer-disabled': {'operation': 'gemm-i8-i8-i64', 'dispatchable': False},
    'q8': {'operation': 'q8_0-f32-matmul', 'dispatchable': False},
    'unknown': {'operation': 'future-operation'},
    'malformed': ['gemm-i8-i8-i64'],
}
print(json.dumps({'gemm': plans[os.environ.get('SHIELD_LIFETIME_SOURCE_PLAN', 'integer')]}))
''')
        subprocess.run(['make', 'all',
            'CPPFLAGS=-DSHIELD_COMPILER=\\"' + str(cls.helper) + '\\"'],
            cwd=cls.work, check=True, stdout=subprocess.DEVNULL)
        subprocess.run(['g++', '-std=c++20', '-Wall', '-Wextra', '-Werror',
            '-fPIC', '-shared', '-pthread', '-DSHIELD_TEST_DRIVER',
            str(ROOT / 'test_program_lifetime.cpp'),
            '-Wl,-soname,libOpenCL-shield-cpu.so.1', '-o',
            str(cls.work / 'runtime/libOpenCL-shield-cpu.so.1')], check=True)
        subprocess.run(['g++', '-std=c++20', '-Wall', '-Wextra', '-Werror',
            str(ROOT / 'test_program_lifetime.cpp'), '-L' + str(cls.work / 'runtime'),
            '-lOpenCL', '-l:libOpenCL-shield-cpu.so.1', '-pthread',
            '-o', str(cls.work / 'client')], check=True)

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def run_case(self, mode, expected):
        log = self.work / (mode + '.jsonl')
        environment = dict(os.environ, LD_LIBRARY_PATH=str(self.work / 'runtime'),
                           SHIELD_LIFETIME_LOG=str(log))
        if mode == 'overlap-build':
            environment['SHIELD_LIFETIME_BARRIER'] = str(self.work / 'barrier')
        subprocess.run([str(self.work / 'client'), mode], env=environment,
                       timeout=10, check=True)
        actual = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
        self.assertEqual(actual, [{'modules': [marker]} for marker in expected])

    def test_recycled_source_cannot_inherit_old_il(self):
        self.run_case('replace-source', ['11'])

    def test_old_release_cannot_erase_new_il(self):
        self.run_case('replace-il', ['11', '22'])

    def test_retained_program_keeps_provenance_until_final_release(self):
        self.run_case('retain', ['11'])

    def test_late_analysis_cannot_authorize_a_newer_build(self):
        self.run_case('overlap-build', ['11'])

    def test_late_compile_cannot_authorize_a_newer_compile(self):
        self.run_case('overlap-compile', [])

    def test_link_cannot_use_recompiled_input_provenance(self):
        self.run_case('overlap-link', [])

    def test_unchanged_compiled_input_can_authorize_link(self):
        self.run_case('stable-link', ['11'])

    def test_cached_plans_preserve_operation_and_dispatchability(self):
        for plan in ('integer', 'integer-disabled', 'q8', 'unknown', 'malformed'):
            with self.subTest(plan=plan):
                log = self.work / ('cache-' + plan + '.jsonl')
                environment = dict(os.environ, LD_LIBRARY_PATH=str(self.work / 'runtime'),
                                   SHIELD_LIFETIME_LOG=str(log), SHIELD_LIFETIME_SOURCE_PLAN=plan)
                subprocess.run([str(self.work / 'client'), 'source-plan-cache'], env=environment,
                               timeout=10, check=True)
                requests = [json.loads(line) for line in log.read_text().splitlines()]
                self.assertEqual(requests, [{'source': 'kernel void gemm() {}', 'options': '-cl-std=CL1.2'}])


if __name__ == '__main__':
    unittest.main()
