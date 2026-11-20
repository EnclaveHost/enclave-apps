#!/usr/bin/env python3
"""Host-container probe: the two Q8 kernels through the service and executor.

Run inside the desktop OCI image with the dedicated test worker on loopback
9741, this tree at /src, the public fixture (shape.txt, quants.bin, scales.bin,
input.bin, native-reference.bin) and the built test-q8-masked at /evidence;
test-q8-identity there is run as well when present.
With `corrupt` or `disconnect` a fault proxy sits in front of the worker and the
launch must fail, leave its output untouched and retire the service.
SHIELD_OPENCL_Q8_DIRECT=0 in the environment selects the queued route.
Public fixtures only; not confidential-VM evidence.
"""
import sys
sys.path.insert(0, '/src')
import json
import os
from pathlib import Path
import subprocess
import threading

from service import Compute, Server, Handler

FIXTURE_MACS = 3 * 896 * 4864


def main():
    fault = sys.argv[1] if len(sys.argv) > 1 else None
    if fault not in (None, 'corrupt', 'disconnect'):
        raise SystemExit('usage: probe_q8_masked.py [corrupt|disconnect]')
    config = json.loads(Path('/shield/service.json').read_text())
    config.update(jobs='/tmp', enableQ8Experimental=True, timeoutSeconds=90, clientUids=[os.getuid()])
    if os.environ.get('SHIELD_PROBE_SDK'):
        config['sdk'] = os.environ['SHIELD_PROBE_SDK']
    proxy = None
    if fault:
        sys.path.insert(0, '/src/native')
        from test_wide import Proxy
        proxy = Proxy(1, fault_kind=fault)
    config['worker'] = {'host': '127.0.0.1', 'port': proxy.port if proxy else 9741}
    # A small card share: the fixture needs one feed-forward tensor, and the
    # fault proxy does not carry frames as large as a vocabulary projection.
    for spec in config['models'].values():
        spec['reserveBytes'] = 128 << 20
    compute = Compute(config)
    Path('/run/shield').mkdir(exist_ok=True)
    server = Server('/run/shield/compute.sock', Handler)
    server.compute = compute
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    results = []
    try:
        for variant in (('mm',) if fault else ('mm', 'mv')):
            env = {'PATH': '/usr/bin:/bin', 'HOME': '/tmp', 'LANG': 'C.UTF-8',
                   'POCL_MAX_PTHREAD_COUNT': '2', 'POCL_CPU_MAX_CU_COUNT': '2',
                   'POCL_CACHE_DIR': '/evidence/opencl-cache',
                   'SHIELD_OPENCL_ARITHMETIC': 'shield-fixed-point-v1'}
            for key in ('SHIELD_OPENCL_Q8_DIRECT', 'SHIELD_OPENCL_Q8_SHARED', 'POCL_DEVICES'):
                if key in os.environ:
                    env[key] = os.environ[key]
            run = subprocess.run(['/evidence/test-q8-masked', '/src/opencl/q8-reference', '/evidence', variant] +
                                 (['--expect-failure'] if fault else []),
                                 env=env, capture_output=True, text=True, timeout=180)
            assert run.returncode == 0, (variant, run.returncode, run.stderr[-2000:])
            result = json.loads(run.stdout)
            assert result['passed']
            if fault:
                assert result['failureLeavesOutputUntouched'] and result['retiredRetryRefused']
            else:
                assert result['matchesNativeFixedPoint'] and result['guards']
            results.append(result)
        identity = None
        if not fault and Path('/evidence/test-q8-identity').exists():
            before = compute.status()['telemetry']['verifiedOffloads']
            run = subprocess.run(['/evidence/test-q8-identity', '/src/opencl/q8-reference', '/evidence'],
                                 env=env, capture_output=True, text=True, timeout=600)
            assert run.returncode == 0, (run.returncode, run.stderr[-2000:])
            identity = json.loads(run.stdout)
            # One verified exchange for every launch of the approved tensor and
            # none for a changed one.
            offloads = compute.status()['telemetry']['verifiedOffloads'] - before
            assert identity['passed'] and offloads == identity['approvedRuns'], (identity, offloads)
        status = compute.status()
        if fault:
            assert status['state'] == 'retired' and status['completed'] == 0, status
            assert status['telemetry']['verifiedOffloads'] == 0, status
            proxy.thread.join(timeout=20)
            assert not proxy.thread.is_alive() and proxy.error is None
            assert (proxy.modified == 1 if fault == 'corrupt' else proxy.dropped == 1)
        else:
            assert status['state'] == 'ready', status
            runs = 2 + (identity['approvedRuns'] if identity else 0)
            assert status['telemetry']['verifiedOffloads'] == runs, status
            assert status['telemetry']['maskedMacs'] == runs * FIXTURE_MACS, status
        print(json.dumps({'schema': 'shield-q8-opencl-masked-probe/2', 'passed': True, 'fault': fault,
                          'route': 'queued' if os.environ.get('SHIELD_OPENCL_Q8_DIRECT') == '0' else 'direct',
                          'cases': results, 'identity': identity, 'status': status}))
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        if compute.q8_broker:
            compute.q8_broker.close()


if __name__ == '__main__':
    main()
