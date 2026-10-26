#!/usr/bin/env python3
"""Container side of an SD fault test; call behind test_wide.Proxy(2).

Runs the image's real service and native engine with a public prompt. The host
must require two matrix replies and exactly one injected corruption. This is
not a test that merely substitutes a subprocess exit status.
"""
import importlib.util
import json
from pathlib import Path
import sys

spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
config = json.loads(Path('/shield/service.json').read_text())
config['worker'] = {'host': '127.0.0.1', 'port': int(sys.argv[1])}
config['jobs'] = '/tmp'
compute = service.Compute(config)
injected = {'seen': False}
if len(sys.argv) > 2:
    original_environment = compute.environment
    def environment(*args):
        result = original_environment(*args)
        result['LD_PRELOAD'] += ':' + sys.argv[2]
        return result
    compute.environment = environment
    original_parse = service.parse_native_stats
    def parse(stderr):
        injected['seen'] = '[test-only] pad exhaustion injected' in stderr
        return original_parse(stderr)
    service.parse_native_stats = parse
request = {'op': 'sd', 'prompt': 'A watercolor cabin beside a lake, pine trees, morning light', 'seed': 42}
if hasattr(service, 'FIXED_ARITHMETIC'):
    request['arithmetic'] = service.FIXED_ARITHMETIC
result = None
try:
    result = compute.run(request)
except ValueError as error:
    message = str(error)
else:
    raise AssertionError('failed masked exchange accepted')
assert compute.retired and compute.completed == 0 and result is None
assert compute.telemetry['verifiedOffloads'] == 0
try:
    compute.run(request)
except ValueError:
    pass
else:
    raise AssertionError('retired service accepted another job')
if len(sys.argv) > 2:
    assert injected['seen'], 'test shim did not reach pad-exhaustion injection'
print(json.dumps({'passed': True, 'state': compute.status()['state'],
                  'completed': compute.completed, 'resultReleased': False, 'error': message,
                  'syntheticPadExhaustionInjected': injected['seen']}))
