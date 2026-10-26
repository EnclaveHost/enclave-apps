#!/usr/bin/env python3
"""Exercise the actual service process and inherited kernel file-size limit.

Synthetic native executable, no models or GPU required. The executable first
completes a control, then writes past the limit. This checks failure propagation,
not native model correctness (covered by test_prompt_boundary.py).
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import socket
import struct
import subprocess
import sys
import tempfile
import time


def call(path, request):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(15)
        sock.connect(str(path))
        data = json.dumps(request).encode()
        sock.sendall(struct.pack('!I', len(data)) + data)
        def exact(size):
            result = b''
            while len(result) < size:
                chunk = sock.recv(size - len(result))
                if not chunk:
                    raise RuntimeError('truncated response')
                result += chunk
            return result
        size, = struct.unpack('!I', exact(4))
        assert 0 < size <= 16 << 20
        return json.loads(exact(size))


def run(service, outer_limit):
    with tempfile.TemporaryDirectory(prefix='shield-file-limit-') as directory:
        root = Path(directory)
        (root / 'bin').mkdir()
        (root / 'jobs').mkdir()
        model, calib, policy = (root / name for name in ('model', 'calib', 'policy'))
        model.write_bytes(b'public synthetic model')
        calib.write_bytes(b'public synthetic calibration')
        sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
        policy.write_text(json.dumps({'modelSha256': sha(model), 'calibrationSha256': sha(calib)}))
        witness = root / 'witness.json'
        launches = root / 'launches'
        stub = root / 'bin/llama-cli'
        stub.write_text('#!/usr/bin/python3\n' +
            'import errno, json, os, pathlib, resource, sys\n' +
            f'with open({str(launches)!r}, "a") as f: f.write("launch\\n")\n' +
            'record = {"limits": resource.getrlimit(resource.RLIMIT_FSIZE)}\n' +
            f'witness = pathlib.Path({str(witness)!r})\n' +
            'witness.write_text(json.dumps(record))\n' +
            'prompt = pathlib.Path(sys.argv[sys.argv.index("-f") + 1]).read_text()\n' +
            'if prompt == "control":\n    print("synthetic positive control")\n' +
            'else:\n' +
            '    written = 0\n' +
            '    try:\n        while True: written += os.write(1, b"x" * 65536)\n' +
            '    except OSError as error:\n' +
            '        record.update(bytesWritten=written, errno=error.errno)\n' +
            '        witness.write_text(json.dumps(record))\n' +
            '        sys.exit(73 if error.errno == errno.EFBIG else 74)\n')
        stub.chmod(0o755)
        config = {'schema': 'shield-guest-service/1', 'sdk': str(root),
            'models': {'llama': {'model': str(model), 'modelSha256': sha(model),
                'policy': str(policy), 'policySha256': sha(policy),
                'calibration': str(calib)}},
            'socket': str(root / 'compute.sock'), 'jobs': str(root / 'jobs'),
            'threads': 1, 'timeoutSeconds': 10, 'clientUids': [os.getuid()]}
        config_path = root / 'config.json'
        config_path.write_text(json.dumps(config))
        # This launcher is single-threaded. Production sets its limit in main
        # before creating service threads, without a child preexec_fn.
        launcher = ('import resource,runpy,sys; '
                    f'resource.setrlimit(resource.RLIMIT_FSIZE, ({outer_limit},{outer_limit})); '
                    'sys.argv=sys.argv[1:]; runpy.run_path(sys.argv[0],run_name="__main__")')
        with (root / 'server.log').open('wb') as log:
            process = subprocess.Popen([sys.executable, '-c', launcher, str(service),
                '--config', str(config_path)], stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 10
                while not Path(config['socket']).exists():
                    assert process.poll() is None, (root / 'server.log').read_text()
                    if time.monotonic() > deadline:
                        raise TimeoutError('service startup')
                    time.sleep(.02)
                request = {'op': 'llama', 'mode': 'cpu', 'prompt': 'control', 'tokens': 1}
                control = call(config['socket'], request)
                assert control['ok'] and 'synthetic positive control' in control['text'], control
                before = call(config['socket'], {'op': 'status'})
                expected = min(8 << 20, outer_limit) if outer_limit >= 0 else 8 << 20
                assert resource.prlimit(process.pid, resource.RLIMIT_FSIZE) == (expected, expected)
                assert json.loads(witness.read_text()) == {'limits': [expected, expected]}
                failed = call(config['socket'], {**request, 'prompt': 'overflow'})
                assert failed == {'ok': False, 'error': 'compute failed; no result released'}, failed
                observed = json.loads(witness.read_text())
                assert observed == {'limits': [expected, expected], 'bytesWritten': expected, 'errno': 27}, observed
                after = call(config['socket'], {'op': 'status'})
                assert after['state'] == 'retired' and after['completed'] == before['completed'] == 1
                assert after['telemetry'] == before['telemetry']
                repeated = call(config['socket'], request)
                assert not repeated['ok'] and 'retired' in repeated['error']
                assert launches.read_text().splitlines() == ['launch', 'launch']
                assert not list((root / 'jobs').iterdir()), 'temporary outputs retained'
                return {'outerHardLimit': outer_limit, 'serviceAndChildLimit': expected,
                    'controlCompleted': True, 'overflow': failed, 'kernelFailureWitness': observed,
                    'successfulCountersPreserved': True, 'repeatLaunchPrevented': True,
                    'temporaryOutputsRemoved': True}
            finally:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--service', type=Path, default=Path(__file__).resolve().parents[1] / 'service.py')
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    records = [run(args.service, limit) for limit in (resource.RLIM_INFINITY, 1 << 20)]
    receipt = {'schema': 'shield-service-file-limit/1',
        'scope': 'real service socket and kernel limits; synthetic native executable', 'cases': records}
    args.output.write_text(json.dumps(receipt, indent=2) + '\n')
    print(json.dumps(receipt))
