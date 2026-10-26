#!/usr/bin/env python3
"""Assemble the separate CUDA/HIP development image from explicit inputs."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--hip-package', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()
source = Path(__file__).resolve().parent
package = args.hip_package / 'opt/shield/hip'
patch_digest = hashlib.sha256((source / 'chipstar-glibc.patch').read_bytes()).hexdigest()
recorded_patch = (package / 'share/shield-build/patch.sha256').read_text().split()[0]
if patch_digest != recorded_patch:
    raise SystemExit('HIP package patch hash does not match the current source')
build_digest = hashlib.sha256((source / 'build.sh').read_bytes()).hexdigest()
recorded_build = (package / 'share/shield-build/build-script.sha256').read_text().split()[0]
if build_digest != recorded_build:
    raise SystemExit('HIP package build script hash does not match the current source')
revisions = json.loads((package / 'share/shield-build/shield-source.json').read_text())
if revisions['.']['revision'] != '3bd6515ebe2a84f2f6d17bee53ed14a5d19a0856':
    raise SystemExit('HIP package source revision mismatch')
output = args.output.resolve()
output.mkdir(parents=True, exist_ok=False)
frontend = output / 'frontend'
frontend.mkdir()
for name in ('Makefile', 'opencl.map', 'shield_opencl.cpp', 'test_opencl.cpp',
             'test_svm.cpp', 'test_event_errors.cpp', 'kernel_compiler.py',
             'gemm_i8.cl', 'gemm_i8_cuda.cu', 'ir_compiler.cpp', 'build-ir-reference.sh'):
    shutil.copy2(source.parent / 'opencl' / name, frontend / name)
shutil.copy2(source / 'test_cuda.cu', frontend / 'test_cuda.cu')
shutil.copy2(source.parent / 'client.py', output / 'client.py')
shutil.copytree(package, output / 'hip', symlinks=True)
shutil.copy2(source / 'desktop.Dockerfile', output / 'Dockerfile')
files = {}
for path in sorted(output.rglob('*')):
    if path.is_symlink():
        files[str(path.relative_to(output))] = {'symlink': str(path.readlink())}
    elif path.is_file():
        files[str(path.relative_to(output))] = hashlib.sha256(path.read_bytes()).hexdigest()
inputs = {'schema': 'shield-hip-image-inputs/1', 'source': revisions,
          'chipstarPatchSha256': patch_digest,
          'buildScriptSha256': build_digest,
          'desktopImageId': subprocess.check_output(['docker', 'image', 'inspect',
              'enclave-kvm-box-desktop:0.5.0-opencl', '--format', '{{.Id}}'], text=True).strip(),
          'files': files}
(output / 'inputs.json').write_text(json.dumps(inputs, indent=2) + '\n')
