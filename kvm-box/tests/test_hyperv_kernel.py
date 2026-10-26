"""Tests for prepared_kernel_v2 on a synthetic tiny source tree.

No real make, bash, compiler or network is used: tool paths point at dummy files and
subprocess.run is replaced by a fake that records calls and writes the final .config.
"""
import contextlib
import hashlib
import io
import json
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import importlib.util
spec = importlib.util.spec_from_file_location("hyperv_kernel_preparation", Path(__file__).resolve().parents[1] / "host/hyperv/prepare-kernel.py")
pk = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pk)

M_TO_Y = ('CONFIGFS_FS', 'HID', 'HID_GENERIC', 'INPUT_EVDEV', 'INPUT_UINPUT', 'NFT_REJECT', 'NFT_REJECT_INET',
          'NFT_REJECT_IPV4', 'NFT_REJECT_IPV6', 'NF_REJECT_IPV4', 'NF_REJECT_IPV6')
BASE = {'LOCALVERSION': '"-microsoft-standard-WSL2"', 'LOCALVERSION_AUTO': 'n', 'PAHOLE_VERSION': '131',
        **{key: 'm' for key in M_TO_Y}, 'UHID': 'n', **{key: 'y' for key in pk.REQUIRED}, 'DEBUG_FS': 'n'}
PREPARED = {**BASE, **{key: 'y' for key in M_TO_Y}, 'UHID': 'y', 'TCG_ENCLAVE_OPENHCL': 'y',
            'LOCALVERSION': '"-enclave-kvm-box"', 'PAHOLE_VERSION': '132'}
DRIVER = b'// synthetic OpenHCL guest TPM transport\n'
SOURCE_FILES = {
    'Makefile': b'# SPDX-License-Identifier: GPL-2.0\nVERSION = 6\nPATCHLEVEL = 6\nSUBLEVEL = 87\n'
                b'EXTRAVERSION = .2\nNAME = synthetic\n',
    'include/config/kernel.release': b'6.6.87.2-microsoft-standard-WSL2\n',
    'scripts/config': b'#!/bin/sh\necho "not to be run in tests" >&2\nexit 1\n',
    'drivers/char/tpm/Kconfig': b'menuconfig TCG_TPM\n\ttristate "TPM Hardware Support"\nif TCG_TPM\nendif # TCG_TPM\n',
    'drivers/char/tpm/Makefile': b'obj-$(CONFIG_TCG_TPM) += tpm.o\n',
}
HOSTILE_ENV = {'MAKEFLAGS': '-j99 O=/tmp/elsewhere', 'O': '/tmp/elsewhere', 'KBUILD_OUTPUT': '/tmp/elsewhere',
               'CROSS_COMPILE': 'evil-', 'KCONFIG_CONFIG': '/tmp/other.config', 'KCONFIG_ALLCONFIG': '/tmp/all',
               'ARCH': 'arm64', 'PATH': '/tmp/evil-bin', 'HOSTCC': '/tmp/evil-cc'}


def sha(data):
    return hashlib.sha256(data).hexdigest()


def config_text(values):
    lines = [f'# CONFIG_{key} is not set' if value == 'n' else f'CONFIG_{key}={value}' for key, value in values.items()]
    return ('#\n# Automatically generated file; DO NOT EDIT.\n#\n' + '\n'.join(lines) + '\n').encode()


class Fixture(unittest.TestCase):
    def setUp(self):
        temp = tempfile.TemporaryDirectory()
        self.addCleanup(temp.cleanup)
        self.base = Path(os.path.realpath(temp.name))
        self.source = self.base / 'source'
        for rel, data in {**SOURCE_FILES, '.config': config_text(BASE)}.items():
            (self.source / rel).parent.mkdir(parents=True, exist_ok=True)
            (self.source / rel).write_bytes(data)
        os.symlink('Kconfig', self.source / 'drivers/char/tpm/Kconfig.alias')
        self.tree = self.base / 'tree'
        self.receipt = self.base / 'receipt.json'
        self.manifest = self.base / 'manifest.json'
        self.driver = self.base / 'driver.c'
        self.driver.write_bytes(DRIVER)
        tools = self.base / 'bin'
        tools.mkdir()
        (tools / 'make').write_bytes(b'dummy make')
        (tools / 'bash').write_bytes(b'dummy bash')
        self.final_config = config_text(PREPARED)
        self.make_error = None
        self.calls = []
        self.status_during_make = None
        for name, value in (('BASE_CONFIG', sha(config_text(BASE))), ('TPM_DRIVER', sha(DRIVER)),
                            ('MAKE', str(tools / 'make')), ('BASH', str(tools / 'bash'))):
            patcher = mock.patch.object(pk, name, value)
            patcher.start()
            self.addCleanup(patcher.stop)
        patcher = mock.patch.object(pk.subprocess, 'run', self.fake_run)
        patcher.start()
        self.addCleanup(patcher.stop)
        self.pin = self.write_manifest()

    def fake_run(self, command, **kwargs):
        self.calls.append((list(command), kwargs))
        if command[-1] == '--version':
            return subprocess.CompletedProcess(command, 0, f'{Path(command[0]).name} 9.9\n', '')
        if command[0] == pk.MAKE:
            self.status_during_make = json.loads(self.receipt.read_bytes())['status']
            if self.make_error:
                raise self.make_error
            Path(kwargs['cwd'], '.config').write_bytes(self.final_config)
        return subprocess.CompletedProcess(command, 0, '', '')

    def write_manifest(self, entries=None, raw=None):
        if raw is None:
            entries = pk.snapshot(self.source) if entries is None else entries
            raw = pk.canonical({'schema': pk.MANIFEST_SCHEMA, 'entries': entries})
        self.manifest.write_bytes(raw)
        return sha(raw)

    def run_helper(self, **overrides):
        args = {'source_tree': self.source, 'tree': self.tree, 'source_manifest': self.manifest,
                'source_manifest_sha256': self.pin, 'driver': self.driver, 'receipt': self.receipt, **overrides}
        argv = []
        for key, value in args.items():
            argv += ['--' + key.replace('_', '-'), str(value)]
        out, err = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            code = pk.main(argv)
        return code, err.getvalue()

    def receipt_json(self):
        return json.loads(self.receipt.read_bytes())

    def assert_refused(self, fragment='', **overrides):
        """Refusal happens before anything is created or any tool is run."""
        code, err = self.run_helper(**overrides)
        self.assertEqual(code, 2, err)
        self.assertIn(fragment, err)
        self.assertFalse(os.path.lexists(overrides.get('tree', self.tree)))
        self.assertFalse(os.path.lexists(overrides.get('receipt', self.receipt)))
        self.assertEqual(self.calls, [])
        return err


class SuccessTests(Fixture):
    def test_success_receipt_binds_prepared_content(self):
        source_before = pk.snapshot(self.source)
        code, err = self.run_helper()
        self.assertEqual(code, 0, err)
        receipt = self.receipt_json()
        self.assertEqual(receipt['status'], 'success')
        self.assertEqual(self.status_during_make, 'preparing')
        self.assertEqual(receipt['preparedSnapshotSha256'], pk.snapshot_digest(self.tree))
        self.assertEqual(receipt['sourceManifestSha256'], self.pin)
        self.assertEqual(receipt['sourceSnapshotSha256'], pk.snapshot_digest(self.source))
        self.assertEqual(receipt['helperSha256'], sha(Path(pk.__file__).read_bytes()))
        self.assertEqual(receipt['tools']['make'], {'path': pk.MAKE, 'sha256': sha(b'dummy make'), 'version': 'make 9.9'})
        self.assertEqual(receipt['configDiff'], {key: list(value) for key, value in pk.EXPECTED_DIFF.items()})
        tpm = self.tree / pk.TPM_DIR
        self.assertEqual((tpm / 'enclave_openhcl_tpm.c').read_bytes(), DRIVER)
        self.assertEqual((tpm / 'Kconfig').read_bytes(), SOURCE_FILES['drivers/char/tpm/Kconfig'] + pk.KCONFIG_ENTRY)
        self.assertEqual(receipt['preparedInputs'], {
            '.config': sha(self.final_config), pk.DRIVER_PATH: sha(DRIVER),
            f'{pk.TPM_DIR}/Kconfig': sha((tpm / 'Kconfig').read_bytes()),
            f'{pk.TPM_DIR}/Makefile': sha((tpm / 'Makefile').read_bytes())})
        self.assertTrue(os.path.islink(tpm / 'Kconfig.alias'))
        self.assertEqual(pk.snapshot(self.source), source_before)
        self.assertEqual(list(self.base.glob('.receipt.json.*')), [])

    def test_minimal_environment_excludes_hostile_variables(self):
        with mock.patch.dict(os.environ, HOSTILE_ENV):
            code, err = self.run_helper()
        self.assertEqual(code, 0, err)
        expected = {'PATH': '/usr/bin:/bin', 'LC_ALL': 'C', 'ARCH': 'x86_64', 'KCONFIG_CONFIG': '.config'}
        self.assertEqual(self.receipt_json()['environment'], expected)
        for command, kwargs in self.calls:
            self.assertEqual(kwargs['env'], expected)
            self.assertTrue(os.path.isabs(command[0]))
        build = [command for command, _ in self.calls if command[-1] != '--version']
        self.assertEqual(len(build), 2)
        self.assertEqual(build[0][:2], [pk.BASH, 'scripts/config'])
        self.assertEqual(build[1], [pk.MAKE, 'olddefconfig'])

    def test_snapshot_cli_output_is_the_pin(self):
        output = self.base / 'generated.json'
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(pk.main(['snapshot', '--source-tree', str(self.source), '--output', str(output)]), 0)
        self.assertEqual(out.getvalue().strip(), sha(output.read_bytes()))
        self.assertEqual(out.getvalue().strip(), self.pin)


class ManifestTests(Fixture):
    def test_manifest_digest_pin_mismatch(self):
        self.assert_refused('digest does not match', source_manifest_sha256='0' * 64)

    def test_manifest_entries_must_describe_actual_source(self):
        def first(entries, kind):
            return next(entry for entry in entries if entry['kind'] == kind)

        cases = {
            'hash': lambda entries: first(entries, 'file').update(sha256='0' * 64),
            'bytes': lambda entries: first(entries, 'file').update(bytes=first(entries, 'file')['bytes'] + 1),
            'mode': lambda entries: first(entries, 'file').update(mode=first(entries, 'file')['mode'] ^ 0o004),
            'directory mode': lambda entries: first(entries, 'directory').update(
                mode=first(entries, 'directory')['mode'] ^ 0o004),
            'symlink target': lambda entries: first(entries, 'symlink').update(target='Makefile'),
            'missing': lambda entries: entries.pop(),
            'extra': lambda entries: entries.append({'path': 'zzz', 'kind': 'file', 'mode': 0o644, 'bytes': 0,
                                                     'sha256': sha(b'')}),
        }
        for name, mutate in cases.items():
            with self.subTest(name):
                entries = pk.snapshot(self.source)
                mutate(entries)
                self.pin = self.write_manifest(entries)
                self.assert_refused('differs from the pinned manifest')

    def test_source_content_change_after_manifest(self):
        (self.source / 'scripts/config').write_bytes(b'#!/bin/sh\necho changed\n')
        self.assert_refused('differs from the pinned manifest')

    def test_source_mode_change_after_manifest(self):
        path = self.source / 'scripts/config'
        os.chmod(path, stat.S_IMODE(os.lstat(path).st_mode) ^ 0o100)
        self.assert_refused('differs from the pinned manifest')

    def test_invalid_manifest_shapes(self):
        file_entry = {'kind': 'file', 'mode': 0o644, 'bytes': 0, 'sha256': sha(b'')}
        good = pk.snapshot(self.source)
        cases = {
            'absolute': [{'path': '/etc/passwd', **file_entry}],
            'dot': [{'path': './Makefile', **file_entry}],
            'dotdot': [{'path': 'a/../Makefile', **file_entry}],
            'empty part': [{'path': 'a//b', **file_entry}],
            'duplicate': [good[0], good[0], *good[1:]],
            'unsorted': list(reversed(good)),
            'unknown kind': [{'path': 'dev', 'kind': 'fifo', 'mode': 0o644}],
            'bool mode': [{'path': 'Makefile', **file_entry, 'mode': True}],
            'missing field': [{'path': 'Makefile', 'kind': 'file', 'mode': 0o644, 'bytes': 0}],
        }
        for name, entries in cases.items():
            with self.subTest(name):
                self.pin = self.write_manifest(entries)
                self.assert_refused('source manifest')
        raw_cases = {
            'wrong schema': json.dumps({'schema': 'other/1', 'entries': good}).encode(),
            'duplicate key': b'{"schema":"kvm-box-kernel-source/1","entries":[],"entries":[]}',
            'not json': b'not json',
        }
        for name, raw in raw_cases.items():
            with self.subTest(name):
                self.pin = self.write_manifest(raw=raw)
                self.assert_refused('source manifest')


class InputPinTests(Fixture):
    def test_base_config_pin_mismatch(self):
        with mock.patch.object(pk, 'BASE_CONFIG', '0' * 64):
            self.assert_refused('base configuration')

    def test_driver_pin_mismatch(self):
        self.driver.write_bytes(DRIVER + b'// tampered\n')
        self.assert_refused('TPM transport differs')

    def test_makefile_version_mismatch(self):
        (self.source / 'Makefile').write_bytes(SOURCE_FILES['Makefile'].replace(b'SUBLEVEL = 87', b'SUBLEVEL = 88'))
        self.pin = self.write_manifest()
        self.assert_refused('SUBLEVEL')

    def test_generated_release_is_a_secondary_check(self):
        (self.source / 'include/config/kernel.release').write_bytes(b'6.6.87.3-microsoft-standard-WSL2\n')
        self.pin = self.write_manifest()
        self.assert_refused('kernel.release')

    def test_already_patched_source_refused(self):
        (self.source / pk.DRIVER_PATH).write_bytes(DRIVER)
        self.pin = self.write_manifest()
        self.assert_refused('already patched')


class SourceShapeTests(Fixture):
    def test_source_symlink_alias_refused(self):
        os.symlink(self.source, self.base / 'alias')
        self.assert_refused('not a symlink', source_tree=self.base / 'alias')

    def test_source_through_symlinked_parent_refused(self):
        os.symlink(self.base, self.base / 'via')
        self.assert_refused('without symlinks', source_tree=self.base / 'via' / 'source')

    def test_external_symlinks_refused(self):
        for name, target in (('absolute', '/etc/passwd'), ('relative', '../manifest.json'), ('deep', 'scripts/../../x')):
            with self.subTest(name):
                link = self.source / 'leak'
                os.symlink(target, link)
                self.assert_refused('symlink leaves the tree')
                os.unlink(link)

    def test_hard_linked_source_file_refused(self):
        os.link(self.source / 'Makefile', self.base / 'Makefile.shared')
        self.assert_refused('hard-linked')

    def test_special_file_refused(self):
        os.mkfifo(self.source / 'fifo')
        self.assert_refused('special file')


class DestinationTests(Fixture):
    def test_existing_tree_not_clobbered(self):
        self.tree.mkdir()
        (self.tree / 'keep').write_bytes(b'caller data')
        code, err = self.run_helper()
        self.assertEqual(code, 2, err)
        self.assertIn('already exists', err)
        self.assertEqual(os.listdir(self.tree), ['keep'])
        self.assertEqual((self.tree / 'keep').read_bytes(), b'caller data')
        self.assertFalse(os.path.lexists(self.receipt))

    def test_dangling_tree_symlink_refused(self):
        os.symlink(self.base / 'nowhere', self.tree)
        code, err = self.run_helper()
        self.assertEqual(code, 2, err)
        self.assertIn('already exists', err)
        self.assertFalse(os.path.lexists(self.base / 'nowhere'))
        self.assertFalse(os.path.lexists(self.receipt))

    def test_existing_receipt_not_clobbered(self):
        self.receipt.write_bytes(b'prior receipt')
        code, err = self.run_helper()
        self.assertEqual(code, 2, err)
        self.assertIn('already exists', err)
        self.assertEqual(self.receipt.read_bytes(), b'prior receipt')
        self.assertFalse(os.path.lexists(self.tree))

    def test_dangling_receipt_symlink_refused(self):
        os.symlink(self.base / 'nowhere', self.receipt)
        code, err = self.run_helper()
        self.assertEqual(code, 2, err)
        self.assertIn('already exists', err)
        self.assertFalse(os.path.lexists(self.base / 'nowhere'))
        self.assertFalse(os.path.lexists(self.tree))

    def test_tree_inside_source_refused(self):
        self.assert_refused('outside --source-tree', tree=self.source / 'copy')

    def test_receipt_inside_source_refused(self):
        self.assert_refused('outside --source-tree', receipt=self.source / 'receipt.json')

    def test_receipt_inside_tree_refused(self):
        self.assert_refused('--receipt', receipt=self.tree / 'receipt.json')

    def test_tree_through_symlinked_parent_refused(self):
        os.symlink(self.base, self.base / 'via')
        self.assert_refused('without symlinks', tree=self.base / 'via' / 'tree')


class FailureTests(Fixture):
    def assert_failed(self, fragment):
        source_before = pk.snapshot(self.source)
        code, err = self.run_helper()
        self.assertEqual(code, 1, err)
        receipt = self.receipt_json()
        self.assertEqual(receipt['status'], 'failed')
        self.assertIn(fragment, receipt['error'])
        self.assertEqual(receipt['retainedTree'], str(self.tree))
        self.assertNotIn('preparedSnapshotSha256', receipt)
        self.assertTrue(self.tree.is_dir())
        self.assertEqual(pk.snapshot(self.source), source_before)
        self.assertEqual(list(self.base.glob('.receipt.json.*')), [])
        return receipt

    def test_config_drift_leaves_failure_receipt(self):
        self.final_config = config_text({**PREPARED, 'DEBUG_FS': 'y'})
        self.assert_failed('DEBUG_FS')
        self.assertEqual((self.tree / pk.DRIVER_PATH).read_bytes(), DRIVER)

    def test_missing_required_diff_fails(self):
        self.final_config = config_text({**PREPARED, 'PAHOLE_VERSION': '131'})
        self.assert_failed('PAHOLE_VERSION')

    def test_localversion_auto_change_fails(self):
        self.final_config = config_text({**PREPARED, 'LOCALVERSION_AUTO': 'y'})
        self.assert_failed('LOCALVERSION_AUTO')

    def test_tool_failure_leaves_failure_receipt(self):
        self.make_error = subprocess.CalledProcessError(2, [pk.MAKE, 'olddefconfig'])
        self.assert_failed('CalledProcessError')


if __name__ == '__main__':
    unittest.main()
