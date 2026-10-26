import importlib.util
import json
import os
from pathlib import Path
import stat
import tempfile
import unittest

APP=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('ui_assets',APP/'guest/native/build_ui_assets.py')
assets=importlib.util.module_from_spec(spec);spec.loader.exec_module(assets)

class Assets(unittest.TestCase):
    def setUp(self):
        self.tmp=tempfile.TemporaryDirectory();self.base=Path(self.tmp.name)
        self.root=self.base/'novnc';self.root.mkdir()
        (self.root/'vnc.html').write_text('<html>static fixture</html>')
        for d in ['app','core','vendor']:(self.root/d).mkdir()
        (self.root/'app/a.js').write_text('export const a=1;')
        self.page=self.base/'native.html';self.page.write_text('<html>native fixture</html>')
        self.output=self.base/'output'
    def tearDown(self):
        for p in self.base.rglob('*'):
            if p.is_dir() and not p.is_symlink():p.chmod(0o755)
        self.tmp.cleanup()
    def build(self):return assets.build(self.root,self.page,self.output)
    def test_flatten_internal_alias_and_permissions(self):
        (self.root/'core/alias.js').symlink_to('../app/a.js')
        m=self.build()
        self.assertEqual(len(m['files']),4)
        self.assertEqual((self.output/'novnc/core/alias.js').read_bytes(),(self.root/'app/a.js').read_bytes())
        for p in self.output.rglob('*'):
            self.assertFalse(p.is_symlink())
            self.assertEqual(stat.S_IMODE(p.stat().st_mode),0o555 if p.is_dir() else 0o444)
        self.assertFalse(m['admissionEstablished'])
    def test_external_alias_refused_before_output(self):
        private=self.base/'private';private.write_text('synthetic-private-data')
        (self.root/'app/escape.js').symlink_to(private)
        with self.assertRaises(ValueError):self.build()
        self.assertFalse(self.output.exists())
    def test_directory_alias_cycle_refused(self):
        (self.root/'app/loop').symlink_to('../app',target_is_directory=True)
        with self.assertRaises(ValueError):self.build()
    def test_fifo_refused_without_open_hang(self):
        os.mkfifo(self.root/'app/pipe')
        with self.assertRaises(ValueError):self.build()
    def test_existing_output_preserved(self):
        self.output.mkdir();(self.output/'keep').write_text('keep')
        with self.assertRaises(FileExistsError):self.build()
        self.assertEqual((self.output/'keep').read_text(),'keep')
    def test_output_inside_source_refused(self):
        with self.assertRaises(ValueError):assets.build(self.root,self.page,self.root/'new')
    def test_input_change_changes_manifest(self):
        first=self.build(); self.output=self.base/'second'
        (self.root/'app/a.js').write_text('export const a=2;')
        second=self.build()
        self.assertNotEqual(first['files'],second['files'])
    def test_missing_route_tree_refused(self):
        (self.root/'vendor').rmdir()
        with self.assertRaises(FileNotFoundError):self.build()

if __name__=='__main__':unittest.main(verbosity=2)
