#!/usr/bin/env python3
"""Container checks of real bootstrap/CLI refusals and service admission.

This test needs public models at /models. It performs no GPU work and may run
with network disabled. Native positive workloads are tested separately.
"""
import importlib.util
import json
from pathlib import Path
import subprocess
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
service = importlib.util.module_from_spec(spec); spec.loader.exec_module(service)
config = json.loads(Path('/shield/service.json').read_text())
compute = service.Compute(config)
initial = compute.status()
records = []
for name, value in [('missing', None), ('native', service.NATIVE_ARITHMETIC),
                    ('unknown', 'shield-fixed-point-v2'), ('boolean', True)]:
    request = {'op': 'llama', 'prompt': 'The capital of France is', 'mode': 'masked'}
    if value is not None: request['arithmetic'] = value
    with patch.object(service.subprocess, 'Popen') as process:
        try:
            compute.run(request)
        except ValueError as error:
            assert 'requires arithmetic' in str(error)
        else:
            raise AssertionError('missing or invalid arithmetic accepted')
        process.assert_not_called()
    assert compute.status() == initial
    records.append({'case': 'service-'+name, 'rejectedBeforeLaunch': True})

environment = compute.environment(config['models']['llama'], True, '/tmp', 'llama')
for name, value, code in [('missing', None, 78), ('wrong', 'native-model-v1', 78),
                        ('explicit', service.FIXED_ARITHMETIC, 0)]:
    env = dict(environment)
    if value is None: env.pop('ENCLAVE_SHIELD_ARITHMETIC')
    else: env['ENCLAVE_SHIELD_ARITHMETIC'] = value
    process = subprocess.run([config['sdk']+'/bin/llama-cli', '--version'], env=env,
                             capture_output=True, text=True, timeout=30)
    assert process.returncode == code, (name, process.returncode, process.stderr)
    if code:
        assert 'startup refused' in process.stderr
        assert 'public policy authenticated' not in process.stderr
    else:
        assert 'public policy authenticated' in process.stderr
    records.append({'case': 'bootstrap-'+name, 'exitCode': process.returncode})

for name, arguments in [('missing-choice', ['llama', '-p', 'test']),
                        ('conflicting', ['llama', '-p', 'test', '--cpu', '--approximate']),
                        ('graph-approximation', ['graph', '--approximate']),
                        ('status-approximation', ['status', '--approximate'])]:
    process = subprocess.run(['/usr/bin/python3', '/shield/client.py', *arguments],
                             capture_output=True, text=True, timeout=10)
    assert process.returncode == 2 and 'Traceback' not in process.stderr, (name, process)
    records.append({'case': 'cli-'+name, 'exitCode': process.returncode})
print(json.dumps({'schema': 'shield-arithmetic-contract-tests/1', 'cases': records,
                  'noGpuWork': True, 'serviceStateUnchanged': compute.status() == initial}))
