#!/usr/bin/env python3
"""Stage a measured incremental OpenCL application image from verified artifacts.

This does not deploy or publish. It records every supplied binary and recipe,
keeps the trusted service's native backend discovery separate from desktop apps,
and requires an immutable base image ID. Build outputs are never taken from a
running VM or a mutable image tag.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess

from source_declines_package import FRONTEND_SOURCES, verify_build


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--base-image', required=True)
    parser.add_argument('--source-declines-build', type=Path, required=True,
                        help='Checked frontend build directory with rejection-table provenance')
    for name in ('base-sdk', 'sd-library', 'cli-library', 'opencl-plugin',
                 'frontend-dir', 'pocl-library', 'pocl-bitcode', 'executor', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', args.base_image):
        parser.error('base-image must be an immutable image ID')
    base_manifest = json.loads((args.base_sdk / 'files.sha256.json').read_text())
    for name, digest in base_manifest.items():
        if sha(args.base_sdk / name) != digest:
            raise ValueError('base SDK mismatch: ' + name)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    repo = Path(__file__).resolve().parent.parent
    inputs = {}
    _, decline_receipt = verify_build(args.source_declines_build,
                                     args.frontend_dir / 'libOpenCL.so.1', repo / 'opencl')

    def copy(src, dest):
        target = out / 'overlay' / dest.lstrip('/')
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, target)
        target.chmod(0o755 if Path(src).stat().st_mode & 0o111 else 0o644)
        inputs[dest] = {'sourceSha256': sha(src)}
        return target

    for src, dest in (
        (args.sd_library, '/opt/shield/lib/libstable-diffusion.so'),
        (args.cli_library, '/opt/shield/lib/libllama-cli-impl.so'),
        (args.executor, '/opt/shield/bin/shield-q8-executor'),
        (args.opencl_plugin, '/opt/shield/apps/bin/libggml-opencl.so'),
        (args.base_sdk / 'lib/backends/libggml-cpu.so', '/opt/shield/apps/bin/libggml-cpu.so'),
        (args.base_sdk / 'bin/llama-cli', '/opt/shield/apps/bin/llama-cli'),
        (args.base_sdk / 'bin/sd-cli', '/opt/shield/apps/bin/sd-cli'),
        (args.pocl_library, '/opt/shield/pocl/lib/libpocl.so.2'),
        (args.pocl_library, '/opt/shield/pocl/lib/libpocl.so.2.13.0'),
        (args.pocl_bitcode, '/opt/shield/pocl/share/pocl/kernel-x86_64-pc-linux-gnu-x86-64.bc'),
        (args.frontend_dir / 'libOpenCL.so.1', '/opt/shield/opencl/lib/libOpenCL.so.1'),
        (args.frontend_dir / 'libOpenCL-shield-cpu.so.1', '/opt/shield/opencl/lib/libOpenCL-shield-cpu.so.1'),
    ):
        path = copy(src, dest)
        if '/apps/bin/' in dest:
            subprocess.run(['patchelf', '--set-rpath', '$ORIGIN/../../lib', str(path)], check=True)
    for name in ('kernel_compiler.py', 'q8_capture_compiler.py', 'q8_kernel_compiler.py',
                 'q8_admission.py', 'gemm_i8.cl'):
        copy(repo / 'opencl' / name, '/opt/shield/opencl/' + name)
    for path in sorted((repo / 'opencl/q8-reference').iterdir()):
        if path.is_file():
            copy(path, '/opt/shield/opencl/q8-reference/' + path.name)
    for name in ('service.py', 'q8_service.py'):
        copy(repo / name, '/shield/' + name)
    if decline_receipt is not None:
        build = args.source_declines_build
        for name in ('manifest.json', 'source_declines.inc'):
            path = build / 'table' / name
            copy(path, '/opt/shield/standard/source-declines/table/' + path.name)
        for path in sorted((build / 'requests').glob('*.json')):
            copy(path, '/opt/shield/standard/source-declines/requests/' + path.name)
        copy(build / 'build-receipt.json', '/opt/shield/standard/source-declines/build-receipt.json')
        copy(build / 'command.json', '/opt/shield/standard/source-declines/command.json')
        copy(repo / 'native/source_declines_package.py', '/opt/shield/standard/source_declines_package.py')
    copy(repo / 'guest-firewall.nft', '/opt/shield/standard/guest-firewall.nft.in')
    for name in ('llama-cli', 'sd-cli'):
        path = out / 'overlay/usr/local/bin' / name
        path.parent.mkdir(parents=True, exist_ok=True)
        extra = ' --no-kv-offload' if name == 'llama-cli' else ''
        path.write_text('#!/bin/sh\nset -eu\n'
                        'export GGML_SHIELD_NATIVE_CPU_FALLBACK=1\n'
                        'export POCL_MAX_PTHREAD_COUNT="${POCL_MAX_PTHREAD_COUNT:-2}"\n'
                        'export POCL_CPU_MAX_CU_COUNT="${POCL_CPU_MAX_CU_COUNT:-2}"\n'
                        f'exec /opt/shield/apps/bin/{name}{extra} "$@"\n')
        path.chmod(0o755)
    # The measured service is the only process that can authorize actual GPU work.
    # Approximate arithmetic still requires each application to opt in explicitly.
    for path in (out / 'overlay').rglob('*'):
        if not path.is_file() or path.read_bytes()[:4] != b'\x7fELF':
            continue
        dynamic = subprocess.check_output(['readelf', '-d', str(path)], text=True)
        if any(name in dynamic.lower() for name in ('libcuda', 'libvulkan', 'libggml-cuda')):
            raise ValueError('unmasked GPU dependency: ' + str(path))
        if any('GNU_STACK' in line and 'RWE' in line for line in
               subprocess.check_output(['readelf', '-lW', str(path)], text=True).splitlines()):
            raise ValueError('executable stack: ' + str(path))
        if any(marker in dynamic for marker in ('/build/', '/input/', '/probe/')):
            raise ValueError('build-only runtime path: ' + str(path))
    patches = ['llama-desktop-loopback.patch', 'llama-shield-opencl.patch',
               'llama-shield-cpu-fallback.patch', 'llama-opencl-q8-views.patch',
               'llama-shield-source-cache.patch',
               'sd-opencl-fallback.patch', 'sd-opencl-graph-operations.patch',
               'sd-opencl-upload.patch', 'sd-graph-diagnostic.patch', 'sd-opencl-capabilities.patch']
    provenance = {'schema': 'shield-standard-package/1', 'baseImage': args.base_image,
                  'baseSdkManifestSha256': sha(args.base_sdk / 'files.sha256.json'),
                  'inputs': inputs, 'patches': {n: sha(repo / 'native' / n) for n in patches},
                  'recipeSha256': {name: sha(repo / 'native' / name) for name in
                                   ('package-standard.py', 'finalize-standard.py', 'build-standard-image.py')},
                  'frontendSources': {name: sha(repo / 'opencl' / name) for name in FRONTEND_SOURCES},
                  'arithmetic': 'explicit shield-fixed-point-v1; otherwise trusted CPU',
                  'deploymentStatus': 'candidate; requires packaged and confidential VM tests'}
    if decline_receipt is not None:
        provenance['sourceDeclines'] = decline_receipt
        provenance['recipeSha256']['source_declines_package.py'] = sha(repo / 'native/source_declines_package.py')
        provenance['recipeSha256']['build-declines-frontend.py'] = sha(repo / 'native/build-declines-frontend.py')
    (out / 'inputs.json').write_text(json.dumps(provenance, indent=2) + '\n')
    (out / 'overlay-files.json').write_text(json.dumps({
        '/' + str(p.relative_to(out / 'overlay')): sha(p)
        for p in sorted((out / 'overlay').rglob('*')) if p.is_file()}, indent=2) + '\n')
    shutil.copyfile(repo / 'native/finalize-standard.py', out / 'finalize-standard.py')
    # BuildKit requires a named local reference rather than a config image ID.
    # build-standard-image.py resolves this content-named alias and verifies the
    # resulting rootfs layer prefix against the requested immutable base.
    alias = 'enclave-kvm-standard-base:' + args.base_image.removeprefix('sha256:')
    (out / 'Dockerfile').write_text(f'FROM {alias}\n'
        'COPY overlay /\nCOPY inputs.json overlay-files.json /opt/shield/standard/\n'
        'COPY finalize-standard.py /tmp/finalize-standard.py\n'
        'RUN python3 -I /tmp/finalize-standard.py && rm /tmp/finalize-standard.py\n')
    print(out)


if __name__ == '__main__':
    main()
