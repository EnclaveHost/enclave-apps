#!/usr/bin/env python3
"""Export pinned clean Git objects, independently of checkout modifications."""
import argparse
import hashlib
import io
import json
from pathlib import Path
import subprocess
import tarfile

REVISIONS = {
    '.': '3bd6515ebe2a84f2f6d17bee53ed14a5d19a0856',
    'HIP': 'a3670d2b094606bf000cea3503756d45d5feaaf7',
    'HIPCC': '5885607ac632a0165bddbe6aca1507e7629abdb9',
    'bitcode/ROCm-Device-Libs': 'f5ad181fd97ede7973466b292219ebe9d0a7f4d6',
    'hip-tests': 'bbbfe89edb2386664395677cff46b59abed2904d',
    'hip-testsuite': '9b9ebd18fb504a467a5e13a5a3c0a0f926197292',
}


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--checkout', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    # Refuse to merge with an older or partially patched export.
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = {}
    for directory, revision in REVISIONS.items():
        archive = subprocess.check_output([
            'git', '-C', str(args.checkout / directory), 'archive', '--format=tar', revision])
        target = args.output / directory
        target.mkdir(parents=True, exist_ok=True)
        with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
            tar.extractall(target, filter='data')
        manifest[directory] = {'revision': revision, 'archiveSha256': hashlib.sha256(archive).hexdigest()}
    (args.output / 'shield-source.json').write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    main()
