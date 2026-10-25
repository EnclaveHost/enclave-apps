#!/usr/bin/env python3
"""Build the pinned public CTS tools offline with a bounded resource budget."""
import argparse
import json
from pathlib import Path
import subprocess
import uuid

p = argparse.ArgumentParser()
p.add_argument('--cts', type=Path, required=True)
a = p.parse_args()
root = a.cts.resolve()
lock = json.loads(Path(__file__).with_name('cts-lock.json').read_text())
if json.loads((root/'source.json').read_text()) != lock['source']:
    raise SystemExit('Unexpected CTS source provenance')
if json.loads((root/'dependencies.json').read_text()) != lock['dependencies']:
    raise SystemExit('Unexpected CTS dependencies')
if (root/'build').exists():
    raise SystemExit('Use a new build directory; preserve previous evidence')
name = 'shield-cts-build-'+uuid.uuid4().hex[:12]
script = '''set -eu
cmake -S source -B build -G Ninja -DCMAKE_BUILD_TYPE=Release -DDEQP_TARGET=x11_egl_glx '-DSELECTED_BUILD_TARGETS=glcts deqp-vk'
cmake --build build --target glcts --parallel 2
cmake --build build --target deqp-vk --parallel 1
sha256sum build/external/openglcts/modules/glcts build/external/vulkancts/modules/vulkan/deqp-vk
'''
command = ['docker', 'run', '--rm', '--name', name, '--network', 'none', '--cpus', '2',
           '--memory', '4g', '--memory-swap', '4g', '--user', '1000:1000',
           '-v', str(root)+':/work', '-w', '/work', lock['toolsImage'], 'sh', '-c', script]
(root/'build-invocation.json').write_text(json.dumps(command, indent=2)+'\n')
try:
    with (root/'build.log').open('x') as log:
        result = subprocess.run(command, stdout=log, stderr=subprocess.STDOUT)
finally:
    subprocess.run(['docker', 'rm', '-f', name], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
raise SystemExit(result.returncode)
