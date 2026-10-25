"""Portable C++/Python rejection-key regression tests; needs g++ and libcrypto.

Run in the OpenCL builder with: python3 -m unittest test_source_declines -v
Application/compiler-corpus and fault tests are separate integration checks.
"""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

from build_source_declines import fingerprint

ROOT = Path(__file__).resolve().parent


class RejectionKeys(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.driver = self.root / 'driver.cpp'
        self.driver.write_text('''#include <iostream>
#include <string>
#include <nlohmann/json.hpp>
#include "source_declines.hpp"
int main() {
  std::string line;
  while (std::getline(std::cin, line)) {
    auto request = nlohmann::json::parse(line);
    std::cout << shield_source_declines::contains(
      request["source"].get<std::string>(), request["options"].get<std::string>()) << "\\n";
  }
}
''')

    def compile_table(self, keys):
        table = self.root / 'table.inc'
        table.write_text('inline constexpr std::array<std::string_view, '
                         + str(len(keys)) + '> fingerprints = {'
                         + ','.join(json.dumps(k) for k in keys) + '};\n')
        return subprocess.run([os.environ.get('CXX', 'g++'), '-std=c++20', '-O2',
                               '-Wall', '-Wextra', '-Werror', '-I' + str(ROOT),
                               '-DSHIELD_SOURCE_DECLINES=' + json.dumps(str(table)),
                               str(self.driver), '-lcrypto', '-o', str(self.root / 'lookup')],
                              capture_output=True, text=True, timeout=60)

    def test_exact_utf8_and_field_boundaries(self):
        known = [('', ''), ('α🌻', '-Dπ'), ('ab', 'c'), ('a\0', 'b')]
        result = self.compile_table(sorted(fingerprint(*pair) for pair in known))
        self.assertEqual(result.returncode, 0, result.stderr)
        cases = [(pair, True) for pair in known] + [(pair, False) for pair in
                 [('a', 'bc'), ('a', '\0b'), ('', ' '), ('α🌻 ', '-Dπ'),
                  ('α🌻', '-Dπ '), ('α🌻/*x*/', '-Dπ'), ('ab', 'c\0')]]
        result = subprocess.run([str(self.root / 'lookup')],
                                input=''.join(json.dumps({'source': s, 'options': o}) + '\n'
                                              for (s, o), _ in cases),
                                capture_output=True, text=True, check=True, timeout=10)
        self.assertEqual(result.stdout.splitlines(), [str(int(expected)) for _, expected in cases])

    def test_invalid_tables_fail_compilation(self):
        for keys in (['g' * 64], ['a' * 63], ['b' * 64, 'a' * 64], ['a' * 64, 'a' * 64]):
            with self.subTest(keys=keys):
                result = self.compile_table(keys)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('decline hints must be sorted unique SHA-256 strings', result.stderr)

    def test_empty_table_never_matches(self):
        result = self.compile_table([])
        self.assertEqual(result.returncode, 0, result.stderr)
        result = subprocess.run([str(self.root / 'lookup')],
                                input='{"source":"","options":""}\n',
                                capture_output=True, text=True, check=True, timeout=10)
        self.assertEqual(result.stdout, '0\n')


if __name__ == '__main__':
    unittest.main()
