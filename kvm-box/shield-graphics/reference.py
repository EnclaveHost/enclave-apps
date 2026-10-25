#!/usr/bin/env python3
"""Build or probe the isolated CPU graphics reference; never deploy or publish it."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import uuid

HERE = Path(__file__).resolve().parent
def sha(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()

def output(command):
    return subprocess.check_output(command, text=True).strip()

def image_id(value):
    if not re.fullmatch(r'sha256:[0-9a-f]{64}', value):
        raise ValueError('an immutable local image ID is required')
    actual = output(['docker', 'image', 'inspect', value, '--format', '{{.Id}}'])
    if actual != value:
        raise ValueError('local image differs from its pin')
    return actual

def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('mode', choices=['build', 'probe'])
    p.add_argument('--image', required=True, help='immutable base image for build, or reference image for probe')
    p.add_argument('--output', type=Path, required=True, help='new directory for receipts and evidence')
    args = p.parse_args()
    pin = image_id(args.image)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    receipt = {'schema': 'shield-graphics-reference/1', 'mode': args.mode, 'inputImage': pin,
               'execution': 'CPU software reference', 'maskedHardwareAcceleration': False,
               'sources': {f.name: sha(f) for f in HERE.iterdir() if f.is_file()}}
    try:
        if args.mode == 'build':
            context = out / 'context'
            context.mkdir()
            shutil.copyfile(HERE / 'reference.Dockerfile', context / 'Dockerfile')
            # BuildKit does not accept a bare local image ID in FROM. Create a
            # dedicated content-named alias, and verify it before the build.
            alias = 'enclave-kvm-graphics-base:' + pin.split(':')[1]
            subprocess.run(['docker', 'tag', pin, alias], check=True)
            if output(['docker', 'image', 'inspect', alias, '--format', '{{.Id}}']) != pin:
                raise ValueError('base alias changed before build')
            command = ['docker', 'build', '--pull=false', '--iidfile', str(out / 'image.id'),
                       '--build-arg', 'BASE_IMAGE=' + alias, str(context)]
            receipt['command'] = command
            with (out / 'build.log').open('w') as log:
                subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True)
            receipt['imageId'] = image_id((out / 'image.id').read_text().strip())
        else:
            # Run as the caller so only this task's evidence directory is
            # writable. No host display, network or GPU devices are forwarded.
            container = 'shield-graphics-probe-' + uuid.uuid4().hex[:16]
            command = ['docker', 'run', '--name', container, '--rm', '--init', '--network', 'none',
                       '--user', f'{os.getuid()}:{os.getgid()}', '--read-only', '--cpus', '2', '--memory', '2g',
                       '--tmpfs', '/tmp:rw,mode=1777',
                       '--mount', f'type=bind,source={HERE},target=/probe,readonly',
                       '--mount', f'type=bind,source={out},target=/evidence',
                       '--entrypoint', '/bin/sh', pin, '/probe/probe-reference.sh']
            receipt['command'] = command
            try:
                with (out / 'probe.log').open('w') as log:
                    subprocess.run(command, stdout=log, stderr=subprocess.STDOUT, check=True, timeout=45)
            finally:
                # A client timeout does not prove its container stopped.
                subprocess.run(['docker', 'rm', '-f', container], stdout=subprocess.DEVNULL,
                               stderr=subprocess.DEVNULL, timeout=15)
            vk = (out / 'vulkan-summary.txt').read_text()
            gl = (out / 'opengl-zink.txt').read_text()
            if 'PHYSICAL_DEVICE_TYPE_CPU' not in vk or 'Accelerated: no' not in gl or 'zink Vulkan' not in gl:
                raise ValueError('unexpected graphics device or renderer')
            receipt['renderedFrames'] = {name: json.loads((out / (name + '.json')).read_text())
                                        for name in ['vulkan-cube', 'opengl-gears']}
            receipt['evidenceSha256'] = {f.name: sha(f) for f in out.iterdir() if f.is_file()}
        receipt['ok'] = True
    except Exception as exc:
        receipt['ok'] = False
        receipt['error'] = str(exc)
        raise
    finally:
        (out / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps({'ok': True, 'receipt': str(out / 'receipt.json'), 'imageId': receipt.get('imageId', pin)}))

if __name__ == '__main__':
    main()
