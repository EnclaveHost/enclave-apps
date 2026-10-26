#!/usr/bin/env python3
"""Launch selected Khronos tests with pinned Mesa and no physical GPU or network."""
import argparse
import json
from pathlib import Path
import subprocess
import uuid

p = argparse.ArgumentParser()
p.add_argument('--cts', type=Path, required=True, help='Directory containing source/ and build/')
p.add_argument('--mesa', type=Path, required=True)
p.add_argument('--output', type=Path, required=True, help='New directory, never overwritten')
p.add_argument('--api', choices=['gl', 'vk'], required=True)
p.add_argument('--group', action='append', default=[])
p.add_argument('--gl-context', choices=['egl', 'glx'], default='egl')
a = p.parse_args()
source = Path(__file__).resolve().parent
lock = json.loads((source/'cts-lock.json').read_text())
cts, mesa, output = a.cts.resolve(), a.mesa.resolve(), a.output.resolve()
if not (cts/'build').is_dir() or not (cts/'source').is_dir():
    raise SystemExit('CTS source and build directories are required')
if json.loads((cts/'source.json').read_text()) != lock['source']:
    raise SystemExit('Unexpected CTS source provenance')
if json.loads((cts/'dependencies.json').read_text()) != lock['dependencies']:
    raise SystemExit('Unexpected CTS dependencies')
output.mkdir(parents=True, exist_ok=False)
name = 'shield-cts-'+uuid.uuid4().hex[:12]
command = ['docker', 'run', '--rm', '--init', '--name', name,
           '--network', 'none', '--read-only', '--cpus', '2', '--memory', '3g',
           '--memory-swap', '3g', '--user', '1000:1000', '--cap-drop', 'ALL',
           '--security-opt', 'no-new-privileges', '--tmpfs', '/tmp:rw,mode=1777',
           '--tmpfs', '/run:rw,mode=755', '-v', str(cts)+':/cts:ro',
           '-v', str(mesa)+':/opt/shield/mesa:ro', '-v', str(output)+':/output:rw',
           '-v', str(source)+':/qualification:ro', lock['toolsImage'],
           'python3', '/qualification/cts-probe.py', '--api', a.api,
           '--gl-context', a.gl_context]
for group in a.group:
    command += ['--group', group]
(output/'invocation.json').write_text(json.dumps(command, indent=2)+'\n')
try:
    with (output/'runner.log').open('x') as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
finally:
    subprocess.run(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
raise SystemExit(result.returncode)
