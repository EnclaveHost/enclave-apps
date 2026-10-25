import importlib.util
from pathlib import Path
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('merge_native', Path(__file__).with_name('merge-native-root.py'))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class MergeTests(unittest.TestCase):
    def test_preserves_customization_and_applies_packages(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            base, graphics, custom = [root / name for name in ('base', 'graphics', 'custom')]
            for folder in (base, graphics, custom):
                folder.mkdir()
                (folder / 'lib').mkdir()
                (folder / 'lib/driver').write_text('old')
            (graphics / 'lib/driver').write_text('new')
            (graphics / 'lib/vulkan').write_text('ICD')
            (custom / 'xorg.conf').write_text('dummy display')
            result = module.merge(base, graphics, custom, root / 'output')
            self.assertEqual(result['changedPaths'], 2)
            self.assertEqual((root / 'output/xorg.conf').read_text(), 'dummy display')
            self.assertEqual((root / 'output/lib/driver').read_text(), 'new')
            (custom / 'lib/driver').write_text('custom driver')
            with self.assertRaisesRegex(ValueError, 'conflicts'):
                module.merge(base, graphics, custom, root / 'rejected')
            self.assertFalse((root / 'rejected').exists())

    def test_parent_symlink_and_custom_directory_removal_are_refused(self):
        directory = {'kind': 'directory', 'mode': 0o755}
        file = {'kind': 'file', 'mode': 0o644, 'sha256': 'a' * 64}
        with self.assertRaisesRegex(ValueError, 'parent is not a directory'):
            module.plan({'lib': directory}, {'lib': directory, 'lib/new': file},
                        {'lib': {'kind': 'link', 'target': '/outside'}})
        with self.assertRaisesRegex(ValueError, 'discard custom assets'):
            module.plan({'folder': directory}, {}, {'folder': directory, 'folder/custom': file})

if __name__ == '__main__':
    unittest.main()
