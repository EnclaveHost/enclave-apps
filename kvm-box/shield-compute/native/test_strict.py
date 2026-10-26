#!/usr/bin/env python3
"""Bounded host tests of native failure policy; dedicated worker 9741 only.

Compile and pass test_wide.py first. This adds transport failures, the actual
production maskbank boundary, and synthetic exhaustion propagation through
GGML and the real SD service. No fixture is installed in the desktop image.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

from test_wide import Proxy


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('image', 'builder'):
        parser.add_argument('--' + name, required=True)
    for name in ('headers', 'wide', 'models', 'legacy-sdk', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    source = Path(__file__).resolve().parent
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    build = ['docker', 'run', '--rm', '--memory=1g', '--cpus=1', '--network=none',
             '--user', subprocess.check_output(['id', '-u'], text=True).strip() + ':' +
             subprocess.check_output(['id', '-g'], text=True).strip(),
             '-v', str(source) + ':/tests:ro', '-v', str(args.headers.resolve()) + ':/headers:ro',
             '-v', str(output) + ':/out', '--entrypoint', 'cc', args.builder]
    objects = ('shielded-field shielded-wire shielded-parwork shielded-pads shielded-bank '
               'shielded-http prefix-kv tweetnacl poly1305-donna shielded-simd-avx512 '
               'shielded-simd-generic').split()
    subprocess.run(build + ['-O2', '-fno-math-errno', '-I/headers', '/tests/test_maskbank.c'] +
                   ['/headers/' + name + '.o' for name in objects] +
                   ['-lm', '-lpthread', '-o', '/out/test-maskbank'], check=True)
    subprocess.run(build + ['-O2', '-shared', '-fPIC', '-I/headers',
                   '/tests/test_exhaust_shim.c', '-ldl', '-o', '/out/exhaust.so'], check=True)
    cases = []

    def save(name, process, proxy=None):
        record = {'name': name, 'exitCode': process.returncode,
                  'stdout': process.stdout, 'stderr': process.stderr}
        if proxy:
            proxy.thread.join(20)
            record.update(wireRows=proxy.rows, maskedHashes=proxy.hashes,
                          droppedRequests=proxy.dropped, modifiedReplies=proxy.modified,
                          proxyError=proxy.error, proxyAlive=proxy.thread.is_alive())
        (output / (name + '.json')).write_text(json.dumps(record, indent=2) + '\n')
        assert process.returncode == 0, record
        if proxy:
            assert not proxy.error and not proxy.thread.is_alive(), record
            assert proxy.modified == 0, record
        cases.append(record)
        print(name + ': passed', flush=True)

    run = ['docker', 'run', '--rm', '--memory=1g', '--cpus=1',
           '-v', str(output) + ':/faults:ro']
    save('maskbank-boundary', subprocess.run(run + ['--network=none', '--entrypoint',
         '/faults/test-maskbank', args.image], capture_output=True, text=True, timeout=30))
    save('legacy-backend-refused', subprocess.run(run + ['--network=none',
         '-v', str(source) + ':/tests:ro', '-v', str(args.models.resolve()) + ':/models:ro',
         '-v', str(args.legacy_sdk.resolve()) + ':/legacy:ro', '--entrypoint',
         '/usr/bin/python3', args.image, '/tests/test_legacy_backend.py',
         '/legacy/lib/backends/libggml-shielded.so'], capture_output=True, text=True, timeout=45))
    for kind, index in [('unavailable', 0), ('disconnect', 1), ('disconnect', 2),
                        ('disconnect', 3), ('exhaustion', 0)]:
        proxy = Proxy(index, fault_kind=kind)
        env = {'PATH': '/usr/bin:/bin', 'SHIELDED_HOST': '127.0.0.1',
               'SHIELDED_PORT': str(proxy.port), 'SHIELDED_CALIB': '/test/fixture.calib',
               'SHIELDED_WIDE': '1', 'SHIELDED_MAX_M': '32', 'SHIELDED_MIN_MACS': '0',
               'SHIELDED_REFILL_THREADS': '1', 'SHIELDED_POOL_DEPTH': '32',
               'SHIELDED_RESERVE_BYTES': '134217728'}
        if kind == 'exhaustion':
            env['LD_PRELOAD'] = '/faults/exhaust.so'
        process = subprocess.run(run + ['--network=host', '-v', str(args.wide.resolve()) +
            ':/test:ro', '--entrypoint', '/usr/bin/env', args.image, '-i'] +
            [key + '=' + value for key, value in env.items()] + ['/test/test-wide', '65',
            'exhaustion' if kind == 'exhaustion' else 'transport-failure', str(index)],
            capture_output=True, text=True, timeout=60)
        save(f'backend-{kind}-{index}', process, proxy)
        expected = [32, 32, 1] if kind == 'exhaustion' else [32, 32, 1][:index]
        assert proxy.rows == expected and proxy.dropped == (kind != 'exhaustion')
        if kind == 'exhaustion':
            assert '[test-only] pad exhaustion injected' in process.stderr

    for kind, index in [('unavailable', 0), ('disconnect', 1), ('disconnect', 2), ('exhaustion', 0)]:
        proxy = Proxy(index, fault_kind=kind)
        command = ['docker', 'run', '--rm', '--memory=4g', '--cpus=2', '--network=host',
                   '-v', str(output) + ':/faults:ro', '-v', str(source) + ':/tests:ro',
                   '-v', str(args.models.resolve()) + ':/models:ro', '--entrypoint',
                   '/usr/bin/python3', args.image, '/tests/test_wide_service.py', str(proxy.port)]
        if kind == 'exhaustion':
            command.append('/faults/exhaust.so')
        process = subprocess.run(command, capture_output=True, text=True, timeout=180)
        save(f'service-{kind}-{index}', process, proxy)
        if kind != 'exhaustion':
            assert proxy.dropped == 1 and len(proxy.rows) == index
        else:
            assert proxy.dropped == 0 and proxy.rows
            assert json.loads(process.stdout)['syntheticPadExhaustionInjected']

    image_id = subprocess.check_output(['docker', 'image', 'inspect', '--format',
                                       '{{.Id}}', args.image], text=True).strip()
    names = ('test_strict.py', 'test_wide.py', 'test_wide.cpp', 'test_maskbank.c',
             'test_exhaust_shim.c', 'test_wide_service.py', 'test_legacy_backend.py',
             'shield-strict-offload.patch')
    receipt = {'schema': 'shield-native-strict-tests/1', 'image': args.image,
               'imageId': image_id, 'scope': 'host containers; no CVM attestation',
               'sources': {name: hashlib.sha256((source / name).read_bytes()).hexdigest() for name in names},
               'cases': cases}
    (output / 'results.json').write_text(json.dumps(receipt, indent=2) + '\n')


if __name__ == '__main__':
    main()
