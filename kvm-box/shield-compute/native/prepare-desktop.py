#!/usr/bin/env python3
"""Build a separate desktop OCI context with measured compute/storage policies."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
from urllib.parse import urlparse

here = Path(__file__).resolve().parent
sys.path.insert(0, str(here.parent))
from compiler import constant_digest

p = argparse.ArgumentParser(description=__doc__)
for name in ('sdk', 'policy', 'core', 'volume', 'enclave', 'services', 'output'):
    p.add_argument('--' + name, required=True, type=Path)
a = p.parse_args()
a.output.mkdir(parents=True, exist_ok=False)
shutil.copy2(here / 'desktop.Dockerfile', a.output / 'Dockerfile')
shutil.copytree(a.sdk, a.output / 'sdk')
shutil.copytree(a.policy, a.output / 'policy')
root = a.output / 'shield'
root.mkdir()
for name in ('service.py', 'client.py', 'compiler.py', 'channel.py', 'boundary-check.py', 'guest-sandbox.c',
             'guest-firewall.nft', 'guest-start.sh'):
    shutil.copy2(here.parent / name, root / name)
ports = []
for service in json.loads(a.services.read_text())['services'].values():
    url = urlparse(service['endpoint'])
    if url.scheme != 'https' or url.hostname != '10.0.2.2' or not url.port or url.port == 9741:
        raise ValueError('unexpected attested service endpoint')
    ports.append(str(url.port))
if not ports:
    raise ValueError('missing attested service endpoints')
firewall = root / 'guest-firewall.nft'
firewall.write_text(firewall.read_text().replace('__ATTESTED_SERVICE_PORTS__', ', '.join(sorted(set(ports)))))
shutil.copy2(a.core, a.output / 'sdk/lib/libshield-core.so')
subprocess.run(['cc', '-static', '-O2', '-Wall', '-Wextra',
                str(a.enclave / 'metal/guest/mverity.c'), '-o', str(root / 'mverity')], check=True)
volume = json.loads((a.volume / 'volume.json').read_text())
v = volume['verity']
if v['alg'] != 'sha256' or any(not isinstance(v[k], int) or v[k] <= 0
        for k in ('dataBlockSize', 'hashBlockSize', 'dataBlocks', 'hashStartBlock')):
    raise ValueError('invalid volume geometry')
for k in ('root', 'salt'):
    if len(v[k]) != 64 or any(c not in '0123456789abcdef' for c in v[k]):
        raise ValueError('invalid volume identity')
(root / 'model-volume.json').write_text(json.dumps({'verity': v}, indent=2) + '\n')
(root / 'mount-models.sh').write_text(
    '#!/bin/sh\nset -eu\n'
    'for i in $(seq 1 100); do [ -b /dev/vda ] && break; sleep .1; done\n'
    f'device=$(/shield/mverity shield-models /dev/vda {v["dataBlockSize"]} {v["hashBlockSize"]} '
    f'{v["dataBlocks"]} {v["hashStartBlock"]} sha256 {v["root"]} {v["salt"]})\n'
    'mount -t ext4 -o ro,noload,nodev,nosuid,noexec "$device" /models\n')
models = {}
for engine, policy, model, reserve in (
        ('llama', 'qwen', 'qwen2.5-0.5b-instruct-q8_0.gguf', 1_200_000_000),
        ('sd', 'sd15', 'sd15-q8.gguf', 1_800_000_000)):
    data = (a.policy / (policy + '.json')).read_bytes()
    models[engine] = {'model': '/models/' + model, 'modelSha256': json.loads(data)['modelSha256'],
                      'policy': '/shield/policy/' + policy + '.json',
                      'policySha256': hashlib.sha256(data).hexdigest(),
                      'calibration': '/shield/policy/' + policy + '.calib', 'reserveBytes': reserve}
config = {'schema': 'shield-guest-service/1', 'sdk': '/opt/shield',
          'core': '/opt/shield/lib/libshield-core.so', 'models': models,
          'worker': {'host': '10.0.2.2', 'port': 9741},
          'socket': '/run/shield/compute.sock', 'jobs': '/run/shield/jobs',
          'threads': 2, 'timeoutSeconds': 1200, 'clientUids': [0, 1000],
          'publicConstants': [constant_digest([[1]] * 32)]}
(root / 'service.json').write_text(json.dumps(config, indent=2) + '\n')
print(a.output)
