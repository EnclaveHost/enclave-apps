#!/usr/bin/env python3
"""Build and verify a local standard OpenCL candidate; never deploy or publish."""
import argparse
import json
from pathlib import Path
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--context', type=Path, required=True)
p.add_argument('--tag', required=True)
a = p.parse_args()
context = a.context.resolve()
inputs = json.loads((context / 'inputs.json').read_text())
base_id = inputs['baseImage']
alias = 'enclave-kvm-standard-base:' + base_id.removeprefix('sha256:')
def inspect(ref):
    return json.loads(subprocess.check_output(['docker', 'image', 'inspect', ref], text=True))[0]
base = inspect(base_id)
assert base['Id'] == base_id
subprocess.run(['docker', 'image', 'tag', base_id, alias], check=True)
assert inspect(alias)['Id'] == base_id
subprocess.run(['docker', 'build', '--pull=false', '--network=none', '-t', a.tag, str(context)], check=True)
built = inspect(a.tag)
base_layers = base['RootFS']['Layers']
assert built['RootFS']['Layers'][:len(base_layers)] == base_layers, 'base rootfs changed during build'
(context.parent / 'image-receipt.json').write_text(json.dumps({
    'schema': 'shield-standard-image/1', 'baseImage': base_id, 'image': built['Id'],
    'tag': a.tag, 'baseLayersVerified': True,
    'status': 'candidate; packaged execution and CVM acceptance pending'}, indent=2) + '\n')
print(built['Id'])
