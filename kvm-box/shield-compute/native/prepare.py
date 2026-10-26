#!/usr/bin/env python3
"""Export pinned sources to an isolated build context without changing checkouts."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import tarfile

here = Path(__file__).resolve().parent
lock = json.loads((here / 'sources.lock.json').read_text())
p = argparse.ArgumentParser(description=__doc__)
for name in ('llama', 'sd', 'enclave', 'shield', 'output'):
    p.add_argument('--' + name, type=Path, required=True)
a = p.parse_args()
out = a.output.resolve()
out.mkdir(parents=True, exist_ok=False)
hashes = {}
for name in ('llama', 'sd', 'enclave', 'shield'):
    dest = out / name
    dest.mkdir()
    archive = out / (name + '.tar')
    paths = [] if name in ('llama', 'sd') else ['wasm', 'LICENSE']
    with archive.open('wb') as f:
        subprocess.run(['git', '-C', str(getattr(a, name)), 'archive', lock[name], *paths],
                       stdout=f, check=True)
    hashes[name] = hashlib.sha256(archive.read_bytes()).hexdigest()
    with tarfile.open(archive) as tar:
        tar.extractall(dest, filter='data')
    archive.unlink()
for patch in lock['llamaPatches']:
    path = out / 'enclave/wasm' / ('llamacpp-' + patch + '.patch')
    subprocess.run(['git', 'apply', str(path)], cwd=out / 'llama', check=True)
    hashes[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
for name in ('llama-private-loopback.patch', 'llama-literal-prompt.patch'):
    path = here / name
    subprocess.run(['git', 'apply', str(path)], cwd=out / 'llama', check=True)
    hashes[path.name] = hashlib.sha256(path.read_bytes()).hexdigest()
for name in ('sd-calib-unified.patch', 'shield-wire-name.patch', 'shield-verified-counters.patch', 'shield-strict-offload.patch', 'shield-start-negotiation.patch', 'shield-unix-peer.patch'):
    path = here / name
    subprocess.run(['git', 'apply', str(path)], cwd=out / 'shield', check=True)
    hashes[name] = hashlib.sha256(path.read_bytes()).hexdigest()
# The public fork consumes the same GGML internals as the library it links.
ggml = out / 'sd/ggml'
if ggml.exists():
    shutil.rmtree(ggml)
ggml.symlink_to('../llama/ggml')
for name in ('Dockerfile', 'build.sh', 'sources.lock.json', 'bootstrap.cpp', 'model-policy.py'):
    shutil.copy2(here / name, out / name)
(out / 'source-provenance.json').write_text(json.dumps({
    'lock': lock, 'archiveAndPatchSha256': hashes,
    'recipeSha256': {name: hashlib.sha256((here / name).read_bytes()).hexdigest()
                     for name in ('Dockerfile', 'build.sh', 'bootstrap.cpp', 'prepare.py', 'model-policy.py')},
}, indent=2) + '\n')
print(out)
