import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location('pair_local', Path(__file__).resolve().parents[1] / 'host/pair-local.py')
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class PairingConfigTests(unittest.TestCase):
    def test_replacement_preserves_identity_and_other_hosts(self):
        original = ('[General]\ncertificate=preserve-certificate\nkey=preserve-key\n'
                    '[hosts]\n1\\manualaddress=192.0.2.1\n1\\srvcert=keep\n'
                    '2\\manualaddress=127.0.0.2\n2\\srvcert=old-vm\n'
                    '3\\manualaddress=192.0.2.2\n3\\srvcert=keep-too\nsize=3\n'
                    '[other]\nvalue=unchanged\n')
        actual = module.without_host(original, '127.0.0.2')
        self.assertEqual(actual, original.replace('2\\manualaddress=127.0.0.2\n2\\srvcert=old-vm\n', '')
                         .replace('3\\', '2\\').replace('size=3', 'size=2'))

    def test_missing_host_section_is_unchanged(self):
        original = '[General]\ncertificate=preserve\n'
        self.assertEqual(module.without_host(original, '127.0.0.2'), original)


if __name__ == '__main__':
    unittest.main()
