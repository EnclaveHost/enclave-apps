"""Mutation tests against a real built frontend and its rejection-table corpus."""
import argparse
import json
from pathlib import Path
import shutil
import tempfile
import unittest

from source_declines_package import read_table, sha, verify_build

BUILD = None
BASELINE = None
SOURCE = Path(__file__).resolve().parent.parent / 'opencl'


class ReleaseGate(unittest.TestCase):
    def setUp(self):
        if BUILD is None:
            self.skipTest('supply --build and --baseline-frontend for artifact integration tests')
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.work = Path(temporary.name)
        self.build = self.work / 'build'
        self.build.mkdir()
        for directory in ('table', 'requests'):
            shutil.copytree(BUILD / directory, self.build / directory)
        for name in ('build-receipt.json', 'command.json'):
            shutil.copyfile(BUILD / name, self.build / name)
        self.frontend = BUILD / 'runtime/libOpenCL.so.1'

    def change_json(self, path, action):
        value = json.loads(path.read_text())
        action(value)
        path.write_text(json.dumps(value, indent=2) + '\n')

    def manifest(self, action):
        self.change_json(self.build / 'table/manifest.json', action)

    def receipt(self, action):
        self.change_json(self.build / 'build-receipt.json', action)

    def table(self):
        return read_table(self.build / 'table', self.build / 'requests')

    def test_valid_real_build(self):
        verify_build(self.build, self.frontend, SOURCE)

    def test_noncanonical_header(self):
        p = self.build / 'table/source_declines.inc'
        p.write_text(p.read_text() + '// changed\n')
        with self.assertRaisesRegex(ValueError, 'canonical rejection table'):
            self.table()

    def test_changed_request_bytes(self):
        p = next((self.build / 'requests').glob('*.json'))
        p.write_bytes(p.read_bytes() + b' ')
        with self.assertRaisesRegex(ValueError, 'request digest'):
            self.table()

    def test_changed_fingerprint(self):
        self.manifest(lambda m: m['records'][0].update(fingerprint='0' * 64))
        with self.assertRaisesRegex(ValueError, 'request fingerprint'):
            self.table()

    def test_duplicate_request(self):
        self.manifest(lambda m: m['records'].append(m['records'][0]))
        with self.assertRaisesRegex(ValueError, 'duplicate source filename'):
            self.table()

    def test_extra_corpus_entry(self):
        (self.build / 'requests' / ('0' * 64 + '.json')).write_text('{}')
        with self.assertRaisesRegex(ValueError, 'corpus membership'):
            self.table()

    def test_different_resource_limit(self):
        self.manifest(lambda m: m['environment']['limits'].update(RLIMIT_FSIZE=[1024, 1024]))
        with self.assertRaisesRegex(ValueError, 'different file limits'):
            self.table()

    def test_corrupt_canary_binding(self):
        self.manifest(lambda m: m['canaryResults'][0].update(variant='mm'))
        with self.assertRaisesRegex(ValueError, 'canary result shape'):
            self.table()

    def test_runtime_nul_inclusive_size_limits(self):
        for field, length in (('source', 1 << 20), ('options', 4096)):
            with self.subTest(field=field):
                # Restore the untouched table for each independent mutation.
                shutil.copyfile(BUILD / 'table/manifest.json', self.build / 'table/manifest.json')
                m = json.loads((self.build / 'table/manifest.json').read_text())
                record = m['records'][0]
                p = self.build / 'requests' / record['requestFile']
                request = json.loads((BUILD / 'requests' / record['requestFile']).read_text())
                request[field] = 'x' * length
                p.write_text(json.dumps(request))
                record['requestSha256'] = sha(p)
                (self.build / 'table/manifest.json').write_text(json.dumps(m))
                with self.assertRaisesRegex(ValueError, 'source request limits'):
                    self.table()

    def test_positive_result_cannot_be_a_decline(self):
        def change(m):
            next(r for r in m['records'] if not r['declinedBothRuns'])['declinedBothRuns'] = True
        self.manifest(change)
        with self.assertRaisesRegex(ValueError, 'nonempty decline'):
            self.table()

    def test_frontend_digest(self):
        self.receipt(lambda r: r.update(frontendSha256='0' * 64))
        with self.assertRaisesRegex(ValueError, 'frontend digest'):
            verify_build(self.build, self.frontend)

    def test_substituted_cpu_loader(self):
        runtime = self.work / 'runtime'
        runtime.mkdir()
        shutil.copyfile(self.frontend, runtime / 'libOpenCL.so.1')
        (runtime / 'libOpenCL-shield-cpu.so.1').write_bytes(b'substituted loader')
        with self.assertRaisesRegex(ValueError, 'CPU loader digest'):
            verify_build(self.build, runtime / 'libOpenCL.so.1')

    def test_other_frontend_without_marker(self):
        self.receipt(lambda r: r.update(frontendSha256=sha(BASELINE)))
        with self.assertRaisesRegex(ValueError, 'frontend table marker'):
            verify_build(self.build, BASELINE)

    def test_changed_manifest_cannot_reuse_marker(self):
        self.manifest(lambda m: m.update(scope='changed manifest'))
        self.receipt(lambda r: r.update(tableManifestSha256=sha(self.build / 'table/manifest.json')))
        with self.assertRaisesRegex(ValueError, 'embedded table binding'):
            verify_build(self.build, self.frontend)

    def test_changed_build_command(self):
        p = self.build / 'command.json'
        p.write_text(p.read_text() + '\n')
        with self.assertRaisesRegex(ValueError, 'build command digest'):
            verify_build(self.build, self.frontend)

    def test_stale_compiler_source(self):
        source = self.work / 'project/opencl'
        source.mkdir(parents=True)
        manifest = self.table()
        receipt = json.loads((self.build / 'build-receipt.json').read_text())
        for name in set(manifest['compilerSources']) | set(receipt['sources']):
            target = source / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(SOURCE / name, target)
        native = source.parent / 'native'
        native.mkdir()
        shutil.copyfile(SOURCE.parent / 'native/build-declines-frontend.py', native / 'build-declines-frontend.py')
        p = source / 'q8_kernel_compiler.py'
        p.write_text(p.read_text() + '\n')
        with self.assertRaisesRegex(ValueError, 'stale compiler source'):
            verify_build(self.build, self.frontend, source)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--build', type=Path, required=True)
    parser.add_argument('--baseline-frontend', type=Path, required=True)
    args = parser.parse_args()
    BUILD, BASELINE = args.build.resolve(), args.baseline_frontend.resolve()
    unittest.main(argv=['test_source_declines_package', '-v'])
