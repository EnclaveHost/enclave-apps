#!/usr/bin/env python3
"""Prove bounded pre-work retries and refusal to retry after public upload starts.

Uses the compiled test_wide fixture and the dedicated loopback worker 9741.
The proxy stores only command IDs and counts. No packet contents are recorded.
"""
import argparse
import json
from pathlib import Path
import socket
import struct
import subprocess
import threading

from test_wide import receive


class NegotiationProxy:
    def __init__(self, mode):
        self.mode = mode
        self.connections = []
        self.refusals = self.drops = self.matrices = 0
        self.error = None
        self.stop = threading.Event()
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(4)
        self.listener.settimeout(.1)
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            while not self.stop.is_set():
                try:
                    client, _ = self.listener.accept()
                except socket.timeout:
                    continue
                commands = []
                self.connections.append(commands)
                with client:
                    client.settimeout(10)
                    header = receive(client, 9)
                    command, size = struct.unpack('<BQ', header)
                    assert command == 0 and size <= 256
                    payload = receive(client, size)
                    commands.append(command)
                    limit = 2 if self.mode == 'recover' else 3 if self.mode == 'exhaust' else 0
                    if len(self.connections) <= limit:
                        self.refusals += 1
                        message = b'test pre-work reservation unavailable'
                        client.sendall(struct.pack('<BQ', 1, len(message)) + message)
                        continue
                    if self.mode == 'bad-version':
                        message = b'{"version":[1,0]}'
                        client.sendall(struct.pack('<BQ', 0, len(message)) + message)
                        continue
                    with socket.create_connection(('127.0.0.1', 9741), 10) as worker:
                        worker.settimeout(10)
                        while True:
                            if self.mode == 'upload-drop' and command == 8:
                                self.drops += 1
                                break
                            worker.sendall(header + payload)
                            reply = receive(worker, 9)
                            _, length = struct.unpack('<BQ', reply)
                            assert length <= 16 << 20
                            client.sendall(reply + receive(worker, length))
                            if command in (12, 13):
                                self.matrices += 1
                            try:
                                header = receive(client, 9)
                            except EOFError:
                                break
                            command, size = struct.unpack('<BQ', header)
                            assert size <= 16 << 20
                            payload = receive(client, size)
                            commands.append(command)
        except BaseException as error:
            self.error = repr(error)
        finally:
            self.listener.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--image', required=True)
    parser.add_argument('--wide', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    records = []
    for mode in ('recover', 'exhaust', 'bad-version', 'upload-drop'):
        proxy = NegotiationProxy(mode)
        env = {'PATH': '/usr/bin:/bin', 'SHIELDED_HOST': '127.0.0.1',
               'SHIELDED_PORT': str(proxy.port), 'SHIELDED_CALIB': '/test/fixture.calib',
               'SHIELDED_WIDE': '1', 'SHIELDED_MAX_M': '32', 'SHIELDED_MIN_MACS': '0',
               'SHIELDED_REFILL_THREADS': '1', 'SHIELDED_POOL_DEPTH': '32',
               'SHIELDED_RESERVE_BYTES': '134217728'}
        command = ['docker', 'run', '--rm', '--memory=1g', '--cpus=1', '--network=host',
                   '-v', str(args.wide.resolve()) + ':/test:ro', '--entrypoint',
                   '/usr/bin/env', args.image, '-i']
        command += [key + '=' + value for key, value in env.items()]
        command += ['/test/test-wide', '65', 'success' if mode == 'recover' else 'transport-failure', '0']
        try:
            process = subprocess.run(command, capture_output=True, text=True, timeout=60)
        finally:
            proxy.stop.set()
            proxy.thread.join(15)
        record = {'mode': mode, 'exitCode': process.returncode, 'stdout': process.stdout,
                  'stderr': process.stderr, 'connections': proxy.connections,
                  'refusals': proxy.refusals, 'drops': proxy.drops,
                  'matrixReplies': proxy.matrices, 'proxyError': proxy.error}
        (output / (mode + '.json')).write_text(json.dumps(record, indent=2) + '\n')
        assert process.returncode == 0 and not proxy.error and not proxy.thread.is_alive(), record
        expected = 3 if mode in ('recover', 'exhaust') else 1
        assert len(proxy.connections) == expected, record
        assert proxy.matrices == (3 if mode == 'recover' else 0), record
        assert proxy.refusals == (2 if mode == 'recover' else 3 if mode == 'exhaust' else 0), record
        assert proxy.drops == (1 if mode == 'upload-drop' else 0), record
        assert process.stderr.count('retrying pre-work negotiation') == (2 if expected == 3 else 0), record
        for commands in proxy.connections[:proxy.refusals]:
            assert commands == [0], record
        records.append(record)
        print(mode + ': passed', flush=True)
    (output / 'results.json').write_text(json.dumps({'schema': 'shield-start-negotiation-tests/1',
        'image': args.image, 'cases': records}, indent=2) + '\n')


if __name__ == '__main__':
    main()
