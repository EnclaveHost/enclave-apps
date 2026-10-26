#!/usr/bin/env python3
"""Reject a plain container export before compressing an unusable VM root."""
import tempfile
from pathlib import Path
import unittest
from build_root_image import require_native_desktop_base

class NativeBaseTests(unittest.TestCase):
    def test_customized_base_and_missing_assets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with self.assertRaisesRegex(ValueError, 'xorg.conf'):
                require_native_desktop_base(root)
            (root / 'xorg.conf').write_text('Section "Device"\n Driver "dummy"\nEndSection\n')
            with self.assertRaisesRegex(ValueError, 'model-volume.json'):
                require_native_desktop_base(root)
            (root / 'shield').mkdir()
            (root / 'shield/model-volume.json').write_text('{}\n')
            self.assertIn('xorg.conf', require_native_desktop_base(root))
            (root / 'xorg.conf').unlink()
            (root / 'xorg.conf').symlink_to('/etc/passwd')
            with self.assertRaisesRegex(ValueError, 'xorg.conf'):
                require_native_desktop_base(root)

if __name__ == '__main__':
    unittest.main()
