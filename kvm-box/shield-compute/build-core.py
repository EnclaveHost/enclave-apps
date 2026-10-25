#!/usr/bin/env python3
"""Build an isolated copy of Shield's non-dealer trusted core; no repo mutation."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--enclave', required=True, type=Path)
p.add_argument('--output', required=True, type=Path)
p.add_argument('--native-worker-route', action='store_true',
               help='backport the fixed root-peer Unix route to the pinned legacy core copy')
a = p.parse_args()
src = a.enclave.resolve() / 'wasm/ggml-shielded'
out = a.output.resolve()
out.mkdir(parents=True, exist_ok=False)
build = out / 'source'
build.mkdir()
hashes = {}
for f in sorted(src.iterdir()):
    if f.is_file() and (f.suffix in ('.c', '.h', '.inc') or f.name == 'Makefile'):
        shutil.copy2(f, build / f.name)
        hashes[f.name] = hashlib.sha256(f.read_bytes()).hexdigest()
upstream_hashes = dict(hashes)
patch_hash = None
if a.native_worker_route:
    patch = Path(__file__).resolve().parent / 'native/shield-core-unix-peer.patch'
    subprocess.run(['git', 'apply', '--check', str(patch)], cwd=build, check=True)
    subprocess.run(['git', 'apply', str(patch)], cwd=build, check=True)
    patch_hash = hashlib.sha256(patch.read_bytes()).hexdigest()
    hashes = {name: hashlib.sha256((build / name).read_bytes()).hexdigest() for name in hashes}
subprocess.run(['make', '-C', str(build), '-j4', 'shielded-probe', 'postmortem-selftest'], check=True)
objects = ['shielded-field', 'shielded-wire', 'shielded-tee', 'shielded-parwork',
           'shielded-pads', 'shielded-bank', 'shielded-http', 'prefix-kv',
           'tweetnacl', 'poly1305-donna', 'shielded-simd-avx512', 'shielded-simd-generic']
subprocess.run(['cc', '-shared', '-Wl,-z,noexecstack', '-Wl,-z,relro,-z,now',
                '-o', str(out / 'libshield-core.so'),
                *(str(build / (n + '.o')) for n in objects), '-lm', '-lpthread'], check=True)
for name in ('shielded-probe', 'postmortem-selftest'):
    shutil.copy2(build / name, out / name)
(out / 'provenance.json').write_text(json.dumps({
    'schema': 'shield-compute-build/1', 'dealerMode': False,
    'sourceSha256': hashes,
    'upstreamSourceSha256': upstream_hashes,
    'nativeWorkerRoutePatchSha256': patch_hash,
    'librarySha256': hashlib.sha256((out / 'libshield-core.so').read_bytes()).hexdigest(),
}, indent=2) + '\n')
print(out)
