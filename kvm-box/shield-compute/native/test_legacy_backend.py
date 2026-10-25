#!/usr/bin/env python3
"""Container fixture: new bootstrap must refuse a backend without strict policy."""
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
config = json.loads(Path('/shield/service.json').read_text())
compute = service.Compute(config)
env = compute.environment(config['models']['llama'], True, '/tmp', 'llama')
env['ENCLAVE_SHIELD_BACKEND'] = sys.argv[1]
process = subprocess.run([config['sdk'] + '/bin/llama-cli', '--version'], env=env,
                         capture_output=True, text=True, timeout=30)
assert process.returncode == 78, process
assert 'strict offload policy unavailable' in process.stderr, process
assert 'public policy authenticated' not in process.stderr, process
print(json.dumps({'exitCode': process.returncode, 'stdout': process.stdout,
                  'stderr': process.stderr, 'passed': True}))
