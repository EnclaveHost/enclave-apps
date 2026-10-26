import hashlib
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
import build_iso

class IsoTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dir = Path(self.tmp.name)
        self.source = self.dir/'payload'
        self.data = bytes(range(256))*32
        self.source.write_bytes(self.data)
        self.sha = hashlib.sha256(self.data).hexdigest()

    def test_independent_iso_reader_extracts_exact_payload(self):
        tool = shutil.which('xorriso')
        if not tool:
            self.skipTest('xorriso required for independent format validation')
        iso, extracted = self.dir/'payload.iso', self.dir/'extracted'
        receipt = build_iso.wrap(self.source, iso, self.sha)
        subprocess.run([tool, '-osirrox', 'on', '-indev', str(iso), '-extract', '/PAYLOAD.BIN', str(extracted)], check=True, capture_output=True)
        self.assertEqual(extracted.read_bytes(), self.data)
        self.assertEqual(iso.read_bytes()[receipt['payloadOffset']:], self.data)
        self.assertEqual(hashlib.sha256(iso.read_bytes()).hexdigest(), receipt['containerSha256'])

    def test_header_only_predicts_exact_full_container(self):
        full, partial = self.dir/'full.iso', self.dir/'header'
        a = build_iso.wrap(self.source, full, self.sha)
        b = build_iso.wrap(self.source, partial, self.sha, True)
        self.assertEqual(a['containerSha256'], b['containerSha256'])
        self.assertEqual(full.read_bytes(), partial.read_bytes()+self.data)
        self.assertEqual(len(partial.read_bytes()), 65536)

    def test_wrong_payload_pin_removes_only_new_output(self):
        out = self.dir/'bad.iso'
        with self.assertRaisesRegex(ValueError, 'pin'):
            build_iso.wrap(self.source, out, '0'*64)
        self.assertFalse(out.exists())
        self.assertEqual(self.source.read_bytes(), self.data)

    def test_existing_output_and_symlink_source_refused(self):
        out = self.dir/'existing.iso'
        out.write_bytes(b'keep')
        with self.assertRaises(FileExistsError):
            build_iso.wrap(self.source, out, self.sha)
        self.assertEqual(out.read_bytes(), b'keep')
        link = self.dir/'link'
        link.symlink_to(self.source)
        with self.assertRaises(OSError):
            build_iso.wrap(link, self.dir/'link.iso', self.sha)

    def test_geometry_bounds(self):
        for size in [0, -1, 4095, 8193, (128<<30)+4096, True, '4096']:
            with self.subTest(size=size), self.assertRaises(ValueError):
                build_iso.header(size)
        self.assertEqual(len(build_iso.header(128<<30)), 65536)

    def test_independent_reader_accepts_multiple_extents(self):
        tool = shutil.which('xorriso')
        if not tool:
            self.skipTest('xorriso required')
        size = 128<<30
        iso = self.dir/'sparse.iso'
        with iso.open('wb') as f:
            f.write(build_iso.header(size))
            f.truncate(size+65536)
        p = subprocess.run([tool, '-indev', str(iso), '-lsl', '/PAYLOAD.BIN'], check=True, capture_output=True, text=True)
        self.assertIn(str(size), p.stdout)

if __name__ == '__main__':
    unittest.main()
