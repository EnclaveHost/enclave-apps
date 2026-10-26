#!/usr/bin/env python3
"""Container fixture: two full native SD jobs, including graph transitions.

Mount public models at /models and a writable evidence directory at /evidence.
Only the dedicated host test worker on 9741 is used. This is not CVM evidence.
"""
import base64
import hashlib
import importlib.util
import json
from pathlib import Path
import time

spec = importlib.util.spec_from_file_location('service', '/shield/service.py')
service = importlib.util.module_from_spec(spec)
spec.loader.exec_module(service)
config = json.loads(Path('/shield/service.json').read_text())
config['worker'] = {'host': '127.0.0.1', 'port': 9741}
config['jobs'] = '/tmp'
compute = service.Compute(config)
records = []
for index in range(2):
    start = time.monotonic()
    request = {'op': 'sd', 'mode': 'masked', 'seed': 42,
        'prompt': 'A watercolor cabin beside a lake, pine trees, morning light'}
    if hasattr(service, 'FIXED_ARITHMETIC'):
        request['arithmetic'] = service.FIXED_ARITHMETIC
    result = compute.run(request)
    elapsed = time.monotonic() - start
    assert result.get('ok') and result['mode'] == 'masked'
    if hasattr(service, 'FIXED_ARITHMETIC'):
        assert result['arithmetic'] == {'id': service.FIXED_ARITHMETIC, 'approximate': True,
            'modelSha256': config['models']['sd']['modelSha256'],
            'policySha256': config['models']['sd']['policySha256'],
            'calibrationSha256': compute.calibration_digests['sd']}
    pixels = base64.b64decode(result.pop('png'))
    Path(f'/evidence/sd-masked-{index}.png').write_bytes(pixels)
    assert result['stats'] == {'offloaded': 320, 'local': 0,
        'graph_macs': 181525282816, 'exchanges': 1136,
        'macs': 181525282816, 'verification_failures': 0}, result
    result.update(seconds=elapsed, pngSha256=hashlib.sha256(pixels).hexdigest())
    records.append(result)
assert records[0]['pngSha256'] == records[1]['pngSha256'], records
status = compute.status()
assert status['state'] == 'ready' and status['completed'] == 2
assert status['telemetry']['verifiedOffloads'] == 2272
assert status['telemetry']['maskedMacs'] == 363050565632
receipt = {'schema': 'shield-native-positive-service/1',
           'scope': 'host container, fixed public SD fixture', 'jobs': records, 'status': status}
Path('/evidence/positive-service.json').write_text(json.dumps(receipt, indent=2) + '\n')
print(json.dumps(receipt), flush=True)
