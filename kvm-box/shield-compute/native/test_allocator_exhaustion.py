#!/usr/bin/env python3
"""Run only in the labeled test image with a 16-window real mask allocator.

The worker must be the dedicated test worker on 9741. No error-return shim or
proxy is used. The observed diagnostic originates in the exhausted-counter
branch; native GGML must propagate SH_ERR_EXHAUST and the service must retire.
"""
import importlib.util
import json
from pathlib import Path

spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
config = json.loads(Path('/shield/service.json').read_text())
config['worker'] = {'host': '127.0.0.1', 'port': 9741}
config['jobs'] = '/tmp'
compute = service.Compute(config)
before = compute.status()
observed = {}
parse = service.parse_native_stats


def observe(stderr):
    observed['allocatorExhaustedAt16'] = (
        '[test-only] actual maskbank exhausted: issued=16 capacity=16' in stderr)
    observed['strictBackendRetiredWithExhaustion'] = (
        '[shielded] protected offload failed (-11); backend retired' in stderr)
    observed['publicPolicyAuthenticated'] = 'public policy authenticated' in stderr
    observed['statsBeforeFailure'] = parse(stderr)
    return parse(stderr)


service.parse_native_stats = observe
request = {'op': 'sd', 'mode': 'masked', 'seed': 42,
    'prompt': 'A watercolor cabin beside a lake, pine trees, morning light',
    'arithmetic': service.FIXED_ARITHMETIC}
errors = []
for job in (request, request, {**request, 'mode': 'cpu', 'arithmetic': service.NATIVE_ARITHMETIC}):
    try:
        compute.run(job)
    except ValueError as error:
        errors.append(str(error))
    else:
        raise AssertionError('exhausted or retired service released a result')
assert errors[0] == 'compute failed; no result released', errors
assert all('retired' in error for error in errors[1:]), errors
assert all(observed[key] for key in ('allocatorExhaustedAt16',
    'strictBackendRetiredWithExhaustion', 'publicPolicyAuthenticated')), observed
after = compute.status()
assert after['state'] == 'retired' and after['active'] is None
assert after['completed'] == before['completed'] and after['telemetry'] == before['telemetry']
print(json.dumps({'schema': 'shield-real-allocator-exhaustion/1', 'testOnly': True,
    'scope': 'host container; separate deployed test required',
    'capacity': 16, 'observed': observed, 'errors': errors,
    'resultReleased': False, 'successfulWorkCountersUnchanged': True,
    'state': after['state']}))
