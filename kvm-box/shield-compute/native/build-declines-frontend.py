#!/usr/bin/env python3
"""Build an immutable rejection-hint frontend and its checked build receipt."""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess

from source_declines_package import FRONTEND_SOURCES, read_table, sha, verify_build


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--table', type=Path, required=True)
    parser.add_argument('--requests', type=Path, required=True)
    parser.add_argument('--builder-image', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', args.builder_image):
        parser.error('builder-image must be an immutable image ID')
    actual = subprocess.check_output(['docker', 'image', 'inspect', args.builder_image,
                                     '--format', '{{.Id}}'], text=True).strip()
    if actual != args.builder_image:
        raise ValueError('builder image mismatch')
    manifest = read_table(args.table, args.requests)
    source = Path(__file__).resolve().parent.parent / 'opencl'
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    for directory in ('table', 'requests', 'source'):
        (out / directory).mkdir()
    for name in ('manifest.json', 'source_declines.inc'):
        shutil.copyfile(args.table / name, out / 'table' / name)
    for record in manifest['records']:
        name = record['requestFile']
        shutil.copyfile(args.requests / name, out / 'requests' / name)
    for name in FRONTEND_SOURCES:
        shutil.copyfile(source / name, out / 'source' / name)
    digest = sha(out / 'table/manifest.json')
    flags = (r'CPPFLAGS=-DSHIELD_Q8_CAPTURE -DSHIELD_Q8_MASKED '
             r'-DSHIELD_COMPILER=\"/opt/shield/opencl/q8_capture_compiler.py\" '
             r'-DSHIELD_Q8_CAPTURE_LOG=\"\" '
             r'-DSHIELD_SOURCE_DECLINES=\"/out/table/source_declines.inc\" '
             r'-DSHIELD_DECLINES_MANIFEST_SHA256=\"' + digest + r'\"')
    command = ['docker', 'run', '--rm', '--network', 'none', '--cpus', '2', '--memory', '2g',
               '-v', str(out) + ':/out', '-v', str(out / 'source') + ':/src:ro', '-w', '/src',
               args.builder_image, 'make', 'all', 'OUT=/out/runtime', flags, 'LDLIBS=-lcrypto']
    (out / 'command.json').write_text(json.dumps(command, indent=2) + '\n')
    with (out / 'build.log').open('wb') as log:
        subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
    receipt = {'schema': 'shield-source-declines-build/1', 'builderImage': args.builder_image,
               'frontendSha256': sha(out / 'runtime/libOpenCL.so.1'),
               'cpuLoaderSha256': sha(out / 'runtime/libOpenCL-shield-cpu.so.1'),
               'tableManifestSha256': digest, 'headerSha256': sha(out / 'table/source_declines.inc'),
               'sources': {name: sha(out / 'source' / name) for name in FRONTEND_SOURCES},
               'buildCommandSha256': sha(out / 'command.json'), 'builderRecipeSha256': sha(__file__)}
    (out / 'build-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    verify_build(out, out / 'runtime/libOpenCL.so.1', source)
    print(json.dumps(receipt))


if __name__ == '__main__':
    main()
