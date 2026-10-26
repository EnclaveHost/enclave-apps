#!/usr/bin/env python3
"""Assemble a separate development desktop context from explicit build inputs."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--pocl-package', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()
source = Path(__file__).resolve().parent
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=False)
frontend = output / 'frontend'
frontend.mkdir()
for name in ('Makefile', 'opencl.map', 'shield_opencl.cpp', 'test_opencl.cpp',
             'test_event_errors.cpp', 'kernel_compiler.py', 'gemm_i8.cl'):
    shutil.copy2(source / name, frontend / name)
shutil.copytree(args.pocl_package / 'opt/shield/pocl', output / 'pocl')
shutil.copy2(source.parent / 'service.py', output / 'service.py')
shutil.copy2(source / 'approve-selftest.py', output / 'approve-selftest.py')
shutil.copy2(source / 'desktop.Dockerfile', output / 'Dockerfile')
files = {}
for path in sorted(output.rglob('*')):
    if path.is_file():
        files[str(path.relative_to(output))] = hashlib.sha256(path.read_bytes()).hexdigest()
inputs = {'schema': 'shield-opencl-image-inputs/1',
          'poclRevision': '952bc559f790e5deb5ae48692c4a19619b53fcdc',
          'poclPatchSha256': hashlib.sha256((source / 'pocl-callback-errors.patch').read_bytes()).hexdigest(),
          'desktopImageId': subprocess.check_output(['docker', 'image', 'inspect',
              'enclave-kvm-box-desktop:0.4.0-shield', '--format', '{{.Id}}'], text=True).strip(),
          'files': files}
(output / 'inputs.json').write_text(json.dumps(inputs, indent=2) + '\n')
