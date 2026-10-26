#!/usr/bin/env python3
"""Build the pinned, Shield-only nvtop fork in a bounded Docker container."""
import argparse
import hashlib
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile

PIN = '71094a9f7dfbfe7cbe9faebfdf055e580a034283'
VENDORS = ('NVIDIA AMDGPU RADEON INTEL MSM APPLE PANFROST PANTHOR ASCEND V3D TPU '
           'ROCKCHIP METAX ENFLAME TENSTORRENT IXML').split()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--source', type=Path, required=True, help='official nvtop Git checkout containing the pinned commit')
    p.add_argument('--output', type=Path, required=True, help='new build directory')
    p.add_argument('--builder', default='enclave-kvm-shield-monitor-builder:1')
    a = p.parse_args()
    out = a.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    source, build, package = out/'source', out/'build', out/'package'
    for directory in (source, build, package):
        directory.mkdir()
    archive = subprocess.check_output(['git', '-C', str(a.source.resolve()), 'archive', PIN])
    with tarfile.open(fileobj=io.BytesIO(archive)) as tar:
        tar.extractall(source, filter='data')
    recipe = Path(__file__).resolve().parent
    subprocess.run(['git', 'apply', str(recipe/'nvtop.patch')], cwd=source, check=True)
    shutil.copyfile(recipe/'extract_gpuinfo_shield.c', source/'src/extract_gpuinfo_shield.c')
    docker = ['docker', 'run', '--rm', '--memory=1g', '--cpus=1', '--network=none',
              '--user', f'{os.getuid()}:{os.getgid()}', '-v', f'{source}:/src:ro', '-v', f'{build}:/build',
              a.builder, '-c']
    options = ' '.join(f'-D{vendor}_SUPPORT=OFF' for vendor in VENDORS)
    subprocess.run(docker + [f'cmake -S /src -B /build -DSHIELD_SUPPORT=ON {options} && cmake --build /build -j1 && patchelf --remove-rpath /build/src/nvtop'], check=True)
    binary = package/'usr/local/bin/nvtop'
    binary.parent.mkdir(parents=True)
    shutil.copyfile(build/'src/nvtop', binary)
    binary.chmod(0o755)
    metadata = package/'opt/shield/monitor'
    metadata.mkdir(parents=True)
    shutil.copyfile(source/'COPYING', metadata/'COPYING')
    shutil.copyfile(recipe/'README.md', metadata/'README.md')
    copyright_text = subprocess.check_output(['docker', 'run', '--rm', '--memory=128m', '--cpus=1',
        '--network=none', a.builder, '-c', 'cat /usr/share/doc/libjson-c-dev/copyright'])
    (metadata/'JSON-C-COPYRIGHT').write_bytes(copyright_text)
    # Complete corresponding source and local recipes accompany the binary.
    shutil.copytree(recipe, source/'shield-integration', ignore=shutil.ignore_patterns('__pycache__'))
    with tarfile.open(metadata/'source.tar.xz', 'w:xz') as tar:
        tar.add(source, arcname='nvtop-shield')
    versions = subprocess.check_output(['docker', 'run', '--rm', '--memory=128m', '--cpus=1',
        '--network=none', a.builder, '-c', 'cat /monitor-package-versions.txt'])
    (metadata/'build-packages.txt').write_bytes(versions)
    provenance = {'schema': 'shield-nvtop-build/1', 'upstream': 'https://github.com/Syllo/nvtop',
        'commit': PIN, 'builderImage': subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', a.builder], text=True).strip(),
        'binarySha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
        'recipeSha256': {name: hashlib.sha256((recipe/name).read_bytes()).hexdigest()
                        for name in ('build.py', 'Dockerfile', 'nvtop.patch', 'extract_gpuinfo_shield.c')},
        'sourceArchiveSha256': hashlib.sha256((metadata/'source.tar.xz').read_bytes()).hexdigest(),
        'physicalGpuBackends': [], 'telemetryEndpoint': '/run/shield/compute.sock', 'expectedPeerUid': 1001}
    (metadata/'provenance.json').write_text(json.dumps(provenance, indent=2)+'\n')
    print(json.dumps(provenance, indent=2))


if __name__ == '__main__':
    main()
