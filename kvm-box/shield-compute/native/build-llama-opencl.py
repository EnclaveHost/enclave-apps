#!/usr/bin/env python3
"""Build llama's OpenCL plugin and libllama from the pinned source and the bound patches.

The source is the tree prepare.py exports (the pinned llama revision with the
SDK's own patches). It is copied, the standard-application patches are applied
in LLAMA_PATCHES order, and the two libraries are built in an immutable builder
image with the SDK's options plus the OpenCL backend. The receipt binds the
source tree, every patch, the command and both outputs; package-standard.py
refuses libraries that do not match it. Nothing is deployed or published.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

# Applied in this order. The first four make the Q8 plugin of 0.6.11; the K-quant
# kernels and paths, device-side glue (inert unless GGML_SHIELD_DEVICE_GLUE names
# an operation) and the fused placement in libllama follow.
LLAMA_PATCHES = ('llama-shield-opencl.patch', 'llama-shield-cpu-fallback.patch',
                 'llama-opencl-q8-views.patch', 'llama-shield-source-cache.patch',
                 'llama-shield-iq4-xs.patch', 'llama-device-glue.patch',
                 'llama-shield-fused-cpu.patch')
OUTPUTS = {'libggml-opencl.so': 'bin/libggml-opencl.so', 'libllama.so.0.0.0': 'bin/libllama.so.0.0.0'}
SCRIPT = r'''set -eu
cmake -S /work/source -B /work/build -G Ninja \
    -DCMAKE_BUILD_TYPE=Release -DBUILD_SHARED_LIBS=ON \
    -DGGML_BACKEND_DL=ON -DGGML_CPU_REPACK=OFF -DGGML_NATIVE=OFF -DGGML_AVX=ON -DGGML_AVX2=ON \
    -DGGML_FMA=ON -DGGML_F16C=ON -DGGML_CUDA=OFF -DGGML_VULKAN=OFF \
    -DGGML_OPENCL=ON -DGGML_OPENCL_EMBED_KERNELS=ON -DGGML_OPENCL_USE_ADRENO_KERNELS=OFF \
    -DGGML_OPENCL_TARGET_VERSION=300 \
    -DGGML_BUILD_TESTS=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
    -DLLAMA_BUILD_TOOLS=OFF -DLLAMA_BUILD_SERVER=OFF -DLLAMA_BUILD_APP=OFF \
    -DLLAMA_CURL=OFF -DLLAMA_OPENSSL=OFF \
    -DCMAKE_C_FLAGS=-DGGML_MAX_NAME=128 -DCMAKE_CXX_FLAGS=-DGGML_MAX_NAME=128
cmake --build /work/build --target ggml-opencl llama -j "$JOBS"
'''


def sha(path):
    with Path(path).open('rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def tree(root):
    """One digest over the relative path, kind and content of every entry."""
    digest = hashlib.sha256(b'shield-source-tree-v1\0')
    for path in sorted(Path(root).rglob('*')):
        name = str(path.relative_to(root)).encode()
        if path.is_symlink():
            body = b'L' + str(path.readlink()).encode()
        elif path.is_file():
            body = b'F' + bytes.fromhex(sha(path))
        else:
            continue
        digest.update(len(name).to_bytes(8, 'little') + name + len(body).to_bytes(8, 'little') + body)
    return digest.hexdigest()


def verify(build, native):
    """The receipt of a build directory, checked against its files and today's patches."""
    build, native = Path(build), Path(native)
    receipt = json.loads((build / 'build-receipt.json').read_text())
    if receipt.get('schema') != 'shield-llama-opencl-build/1':
        raise ValueError('llama build receipt schema')
    if re.fullmatch(r'sha256:[0-9a-f]{64}', receipt['builderImage']) is None:
        raise ValueError('llama builder image identity')
    lock = json.loads((native / 'sources.lock.json').read_text())
    if receipt['llamaRevision'] != lock['llama']:
        raise ValueError('llama build is of another pinned revision')
    if list(receipt['patches']) != list(LLAMA_PATCHES):
        raise ValueError('llama patch set or order')
    for name, digest in receipt['patches'].items():
        if sha(native / name) != digest:
            raise ValueError('stale llama patch: ' + name)
    if sha(native / 'build-llama-opencl.py') != receipt['builderRecipeSha256']:
        raise ValueError('stale llama build recipe')
    if sha(build / 'command.json') != receipt['buildCommandSha256']:
        raise ValueError('llama build command digest')
    if set(receipt['outputs']) != set(OUTPUTS):
        raise ValueError('llama build outputs')
    for name, digest in receipt['outputs'].items():
        if sha(build / 'lib' / name) != digest:
            raise ValueError('llama build output digest: ' + name)
    return receipt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True, help="prepare.py's exported llama tree")
    parser.add_argument('--builder-image', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--jobs', type=int, default=8)
    args = parser.parse_args()
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', args.builder_image):
        parser.error('builder-image must be an immutable image ID')
    actual = subprocess.check_output(['docker', 'image', 'inspect', args.builder_image,
                                     '--format', '{{.Id}}'], text=True).strip()
    if actual != args.builder_image:
        raise ValueError('builder image mismatch')
    native = Path(__file__).resolve().parent
    lock = json.loads((native / 'sources.lock.json').read_text())
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    shutil.copytree(args.source, out / 'source', symlinks=True)
    pinned = tree(out / 'source')
    patches = {}
    for name in LLAMA_PATCHES:
        subprocess.run(['git', 'apply', '--whitespace=nowarn', str(native / name)], cwd=out / 'source', check=True)
        patches[name] = sha(native / name)
    (out / 'build').mkdir()
    (out / 'lib').mkdir()
    command = ['docker', 'run', '--rm', '--network', 'none', '--cpus', str(args.jobs), '--memory', '12g',
               '--user', f'{os.getuid()}:{os.getgid()}',
               '-e', 'JOBS=' + str(args.jobs), '-v', str(out / 'source') + ':/work/source:ro',
               '-v', str(out / 'build') + ':/work/build', '--entrypoint', 'sh',
               args.builder_image, '-c', SCRIPT]
    (out / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
    with (out / 'build.log').open('wb') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    for name, built in OUTPUTS.items():
        shutil.copyfile(out / 'build' / built, out / 'lib' / name)
        (out / 'lib' / name).chmod(0o755)
        # The build directory is not a runtime path. The packager gives the plugin its own.
        subprocess.run(['patchelf', '--set-rpath', '$ORIGIN', str(out / 'lib' / name)], check=True)
    receipt = {'schema': 'shield-llama-opencl-build/1', 'builderImage': args.builder_image,
               'llamaRevision': lock['llama'], 'preparedSourceTreeSha256': pinned,
               'patchedSourceTreeSha256': tree(out / 'source'), 'patches': patches,
               'outputs': {name: sha(out / 'lib' / name) for name in OUTPUTS},
               'buildCommandSha256': sha(out / 'command.json'), 'builderRecipeSha256': sha(__file__)}
    (out / 'build-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    verify(out, native)
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
