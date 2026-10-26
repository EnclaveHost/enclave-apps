"""Build-input boundary tests for the immutable native desktop defaults."""
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import tempfile
import unittest

APP = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('home_build', APP/'guest/native/build_home_defaults.py')
home_build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(home_build)


class HomeBuild(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.base = Path(self.tmp.name)
        self.root = self.base/'home'
        self.root.mkdir()
        self.files = {'.fluxbox/menu': b'[begin] (Desktop)\n[end]\n',
                      '.mozilla/firefox/default/user.js': b'user_pref("test", false);\n',
                      '.empty': b''}
        entries = []
        for name, data in self.files.items():
            p = self.root/name
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_bytes(data)
            entries.append({'path':name, 'bytes':len(data), 'mode':0o644,
                            'sha256':hashlib.sha256(data).hexdigest()})
        self.manifest = self.base/'manifest.json'
        self.manifest.write_text(json.dumps({'schema':'kvm-box-native-home-defaults/1','files':entries}))

    def build(self):
        return home_build.build(self.root, self.manifest)

    def test_deterministic_complete_generation(self):
        a = self.build()
        self.assertEqual(a, self.build())
        for name in self.files:
            self.assertIn(json.dumps(name), a)
        self.assertIn('NATIVE_HOME_MANIFEST_SHA256', a)

    def test_changed_or_truncated_file_refused(self):
        p = self.root/'.fluxbox/menu'
        for data in [b'[begin] (Changed)\n[end]\n', b'', b'x'*300000]:
            p.write_bytes(data)
            with self.assertRaises(ValueError): self.build()

    def test_private_state_is_not_imported(self):
        (self.root/'.config/sunshine').mkdir(parents=True)
        (self.root/'.config/sunshine/state.json').write_text('fixture must not be imported')
        with self.assertRaises(ValueError): self.build()

    def test_file_symlink_refused(self):
        p = self.root/'.fluxbox/menu'
        p.rename(self.base/'outside')
        p.symlink_to(self.base/'outside')
        with self.assertRaises(ValueError): self.build()

    def test_directory_symlink_and_root_symlink_refused(self):
        d = self.root/'.fluxbox'
        d.rename(self.base/'outside')
        d.symlink_to(self.base/'outside', target_is_directory=True)
        with self.assertRaises(ValueError): self.build()
        d.unlink()
        (self.base/'outside').rename(d)
        alias = self.base/'alias'
        alias.symlink_to(self.root, target_is_directory=True)
        with self.assertRaises(OSError): home_build.build(alias,self.manifest)

    def test_hardlink_and_fifo_refused(self):
        p = self.root/'.fluxbox/menu'
        os.link(p,self.base/'hardlink')
        with self.assertRaises(ValueError): self.build()
        (self.base/'hardlink').unlink()
        p.unlink()
        os.mkfifo(p)
        with self.assertRaises(ValueError): self.build()

    def test_missing_file_refused(self):
        (self.root/'.empty').unlink()
        with self.assertRaises(ValueError): self.build()

    def test_unsafe_paths_duplicates_and_size_bounds_refused(self):
        original = self.manifest.read_text()
        for name in ['../outside','/absolute','a//b','a/./b','a/../b','a\\b','a"b','a b','a:b','x'*65,'/'.join(['a']*9)]:
            m = json.loads(original)
            m['files'][0]['path'] = name
            self.manifest.write_text(json.dumps(m))
            with self.assertRaises(ValueError): self.build()
        m = json.loads(original)
        m['files'].append(m['files'][0])
        self.manifest.write_text(json.dumps(m))
        with self.assertRaises(ValueError): self.build()
        m = json.loads(original)
        m['files'][0]['bytes'] = 1048577
        self.manifest.write_text(json.dumps(m))
        with self.assertRaises(ValueError): self.build()


if __name__ == '__main__':
    unittest.main(verbosity=2)
