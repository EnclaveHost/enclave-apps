#!/usr/bin/env python3
"""Real-worker chunking and fault tests for the packaged native GGML backend.

Use only the dedicated loopback test worker on port 9741. No production worker
or GPU device is passed into the containers. Host tests are not CVM evidence.
"""
import argparse
import hashlib
import json
from pathlib import Path
import socket
import struct
import subprocess
import threading


def receive(sock, size):
    data = bytearray()
    while len(data) < size:
        part = sock.recv(size - len(data))
        if not part:
            raise EOFError
        data.extend(part)
    return bytes(data)


class Proxy:
    def __init__(self, fault_at, fault_member=0, fault_kind='corrupt'):
        self.fault_at = fault_at
        self.fault_member = fault_member
        self.fault_kind = fault_kind
        self.dropped = 0
        self.rows, self.hashes = [], []
        self.modified = 0
        self.error = None
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(1)
        self.listener.settimeout(30)
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            with self.listener.accept()[0] as client, socket.create_connection(('127.0.0.1', 9741), 15) as worker:
                client.settimeout(15)
                worker.settimeout(15)
                while True:
                    header = receive(client, 9)
                    command, size = struct.unpack('<BQ', header)
                    if size > 16 << 20:
                        raise ValueError('oversized request')
                    payload = receive(client, size)
                    if self.fault_kind == 'unavailable':
                        self.dropped += 1
                        return
                    matrix = command in (12, 13)
                    if matrix:
                        nodes, rows = struct.unpack_from('<II', payload)
                        if not 1 <= nodes <= 8:
                            raise ValueError('invalid group size')
                        self.rows.append(rows)
                        self.hashes.append(hashlib.sha256(payload[8 + 4 * nodes:]).hexdigest())
                        if self.fault_kind == 'disconnect' and len(self.rows) == self.fault_at:
                            self.dropped += 1
                            return
                    worker.sendall(header + payload)
                    header = receive(worker, 9)
                    status, size = struct.unpack('<BQ', header)
                    if size > 16 << 20:
                        raise ValueError('oversized reply')
                    data = receive(worker, size)
                    if matrix and len(self.rows) == self.fault_at and self.fault_kind == 'corrupt':
                        if status or not data:
                            raise ValueError('cannot inject fault into failed reply')
                        # Synthetic grouped fixture has equal-width members.
                        offset = len(data) // nodes * self.fault_member
                        if offset >= len(data):
                            raise ValueError('invalid fault member')
                        data = data[:offset] + bytes([data[offset] ^ 1]) + data[offset+1:]
                        self.modified += 1
                    client.sendall(header + data)
        except EOFError:
            pass
        except BaseException as error:
            self.error = repr(error)
        finally:
            self.listener.close()


def test(image, output, rows, fault_at=0, fault_member=0, mixed=False, unverified=False):
    proxy = Proxy(fault_at, fault_member)
    env = {'SHIELDED_HOST': '127.0.0.1', 'SHIELDED_PORT': str(proxy.port),
           'SHIELDED_CALIB': '/test/fixture.calib', 'SHIELDED_WIDE': '1',
           'SHIELDED_MAX_M': '32', 'SHIELDED_MIN_MACS': '0',
           'SHIELDED_REFILL_THREADS': '1', 'SHIELDED_POOL_DEPTH': '32',
           'SHIELDED_RESERVE_BYTES': '134217728'}
    if mixed:
        env['SHIELDED_WIDE'] = '0'
    command = ['docker', 'run', '--rm', '--memory=1g', '--cpus=1', '--network=host',
               '-v', str(output) + ':/test:ro', '--entrypoint', '/usr/bin/env', image,
               '-i', 'PATH=/usr/bin:/bin']
    command += [f'{key}={value}' for key, value in env.items()]
    mode = 'unverified' if unverified else 'mixed' if mixed else 'failure' if fault_at else 'success'
    command += ['/test/test-wide', str(rows), mode, str(fault_at)]
    process = subprocess.run(command, capture_output=True, text=True, timeout=60)
    proxy.thread.join(20)
    name = f'fixture-{rows}-{fault_at}' + ('-mixed' if mixed else '') + (f'-member{fault_member}' if fault_member else '')
    if unverified:
        name += '-unverified'
    (output / (name + '.stdout')).write_text(process.stdout)
    (output / (name + '.stderr')).write_text(process.stderr)
    record = {'rows': rows, 'faultAt': fault_at, 'exitCode': process.returncode,
              'wireRows': proxy.rows, 'maskedHashes': proxy.hashes,
              'modifiedReplies': proxy.modified, 'proxyError': proxy.error,
              'faultMember': fault_member, 'mixedLocalRemote': mixed,
              'unverifiedDiagnostic': unverified}
    (output / (name + '.json')).write_text(json.dumps(record, indent=2) + '\n')
    if proxy.thread.is_alive() or proxy.error or process.returncode:
        raise RuntimeError(f'{name} failed; inspect saved evidence')
    expected = [32] if mixed else [32] * (rows // 32) + ([rows % 32] if rows % 32 else [])
    if proxy.rows != (expected[:fault_at] if fault_at else expected) or proxy.modified != bool(fault_at):
        raise RuntimeError(f'{name}: wrong chunk or fault count')
    # The C++ fixture repeats its plaintext every 32 rows. Full chunks must
    # therefore still carry distinct masked payloads under fresh pad cells.
    full_hashes = [digest for count, digest in zip(proxy.rows, proxy.hashes) if count == 32]
    if len(full_hashes) != len(set(full_hashes)):
        raise RuntimeError(f'{name}: repeated mask for identical full chunks')
    print(f'{name}: passed', flush=True)
    return record


def service_fault(image, output, models):
    proxy = Proxy(2)
    process = subprocess.run([
        'docker', 'run', '--rm', '--memory=8g', '--cpus=2', '--network=host',
        '-v', str(Path(__file__).parent.resolve()) + ':/tests:ro',
        '-v', str(models.resolve()) + ':/models:ro', '--entrypoint', '/usr/bin/python3',
        image, '/tests/test_wide_service.py', str(proxy.port),
    ], capture_output=True, text=True, timeout=180)
    proxy.thread.join(20)
    record = {'exitCode': process.returncode, 'wireRows': proxy.rows,
              'modifiedReplies': proxy.modified, 'proxyError': proxy.error,
              'stdout': process.stdout, 'stderr': process.stderr}
    (output / 'service-fault.json').write_text(json.dumps(record, indent=2) + '\n')
    if (proxy.thread.is_alive() or proxy.error or process.returncode or
            proxy.rows != [32, 32] or proxy.modified != 1):
        raise RuntimeError('native SD service fault test failed; inspect evidence')
    print('service-fault: passed', flush=True)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--builder', required=True)
    parser.add_argument('--sdk', type=Path, required=True)
    parser.add_argument('--headers', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--models', type=Path, help='also test real SD service retirement')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    source = Path(__file__).with_suffix('.cpp')
    (output / source.name).write_bytes(source.read_bytes())
    subprocess.run(['docker', 'run', '--rm', '--memory=1g', '--cpus=1', '--network=none',
                    '--entrypoint', 'c++', '-v', str(output) + ':/test',
                    '-v', str(args.sdk.resolve()) + ':/sdk:ro',
                    '-v', str(args.headers.resolve()) + ':/headers:ro', args.builder,
                    '-O2', '-std=c++17', '-DGGML_MAX_NAME=128', '-I/sdk/include', '-I/headers',
                    '/test/test_wide.cpp', '-L/sdk/lib', '-L/sdk/lib/backends',
                    '-lggml-shielded', '-lggml', '-lggml-base',
                    '-Wl,-rpath-link,/sdk/lib:/sdk/lib/backends',
                    '-Wl,-rpath,/opt/shield/lib:/opt/shield/lib/backends',
                    '-o', '/test/test-wide'], check=True)
    (output / 'fixture.calib').write_text('# shielded-calib 2\nsite blk.0.ffn_gate.weight 8 0\nsite blk.0.ffn_up.weight 8 0\n')
    records = [test(args.image, output, rows) for rows in (33, 64, 65, 77, 256)]
    records += [test(args.image, output, 65, fault_at) for fault_at in (1, 2, 3)]
    records += [test(args.image, output, 65, fault_at=2, fault_member=1),
                test(args.image, output, 65, mixed=True),
                test(args.image, output, 65, unverified=True)]
    image_id = subprocess.check_output(['docker', 'image', 'inspect', '--format', '{{.Id}}', args.image], text=True).strip()
    receipt = {'schema': 'shield-native-wide-tests/1', 'image': args.image,
               'imageId': image_id, 'sourceSha256': hashlib.sha256(source.read_bytes()).hexdigest(),
               'cases': records}
    if args.models:
        receipt['serviceFault'] = service_fault(args.image, output, args.models)
    (output / 'results.json').write_text(json.dumps(receipt, indent=2) + '\n')


if __name__ == '__main__':
    main()
