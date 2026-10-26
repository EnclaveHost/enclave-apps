#!/usr/bin/env python3
"""Run in the bounded monitor test container as root, never on the host.

Creates a synthetic Unix status server as UID 1001. No GPU, worker or secrets.
The supplied nvtop binary is exercised through its real JSON snapshot mode.
"""
import argparse
import copy
import json
import multiprocessing as mp
import os
from pathlib import Path
import socket
import struct
import subprocess
import time

SOCKET = '/run/shield/compute.sock'
GOOD = {'ok': True, 'schema': 'shield-guest-status/1', 'state': 'ready',
        'completed': 4, 'gpuRoute': 'masked-verified-only',
        'telemetry': {'verifiedOffloads': 1604, 'maskedMacs': 192862767104}}


def exact(sock, length):
    result = b''
    while len(result) < length:
        part = sock.recv(length - len(result))
        if not part:
            raise EOFError
        result += part
    return result


def server(ready, records, replies, uid, slow):
    os.setgid(uid)
    os.setuid(uid)
    Path(SOCKET).unlink(missing_ok=True)
    with socket.socket(socket.AF_UNIX) as listener:
        listener.bind(SOCKET)
        os.chmod(SOCKET, 0o666)
        listener.listen(8)
        listener.settimeout(4)
        ready.set()
        try:
            for reply in replies:
                with listener.accept()[0] as client:
                    client.settimeout(2)
                    try:
                        size, = struct.unpack('!I', exact(client, 4))
                        assert size <= 64
                        request = json.loads(exact(client, size))
                        assert request == {'op': 'status'}, request
                        records.put(request)
                        if slow:
                            time.sleep(1)
                        if reply == 'oversize-frame':
                            client.sendall(struct.pack('!I', 4097))
                        elif reply == 'zero-frame':
                            client.sendall(struct.pack('!I', 0))
                        elif reply == 'truncated-frame':
                            client.sendall(struct.pack('!I', 10) + b'{}')
                        else:
                            body = reply if isinstance(reply, bytes) else json.dumps(reply).encode()
                            client.sendall(struct.pack('!I', len(body)) + body)
                    except (EOFError, BrokenPipeError, ConnectionResetError):
                        pass
        except TimeoutError:
            pass


def test(binary, name, replies, expect=None, uid=1001, slow=False):
    ready, records = mp.Event(), mp.Queue()
    process = mp.Process(target=server, args=(ready, records, replies, uid, slow))
    process.start()
    try:
        assert ready.wait(3), name
        started = time.monotonic()
        result = subprocess.run([binary, '--snapshot'], capture_output=True, text=True, timeout=6)
        elapsed = time.monotonic() - started
        assert result.returncode == 0, (name, result.stderr)
        if expect == 'absent':
            assert result.stdout.strip() == 'No GPU to monitor.', (name, result.stdout)
        else:
            devices = json.loads(result.stdout)
            assert len(devices) == 1, devices
            device = devices[0]
            assert device['device_name'] == 'Enclave Shield Virtual GPU', device
            assert device['shield'] == expect, (name, device)
            for key in ('gpu_util', 'mem_util', 'mem_total', 'mem_used', 'mem_free',
                        'gpu_clock', 'mem_clock', 'temp', 'power_draw', 'fan_speed'):
                assert device[key] is None, (name, key, device[key])
        if slow:
            assert elapsed < 2, (name, elapsed)
        process.join(2)
        if process.is_alive():
            raise AssertionError(f'{name}: fixture did not finish')
        assert process.exitcode == 0, (name, process.exitcode)
        requests = []
        while not records.empty():
            requests.append(records.get())
        assert len(requests) == (0 if uid != 1001 else len(replies)), (name, requests)
        print(name + ': passed', flush=True)
        return {'case': name, 'seconds': elapsed, 'statusRequests': len(requests),
                'snapshot': None if expect == 'absent' else devices}
    finally:
        if process.is_alive():
            process.terminate()
        process.join()
        Path(SOCKET).unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    assert Path('/.dockerenv').exists() and os.geteuid() == 0, 'container-only test'
    Path('/run/shield').mkdir(exist_ok=True)
    os.chown('/run/shield', 1001, 1001)
    cases = []
    for state in ('ready', 'busy', 'retired'):
        value = copy.deepcopy(GOOD)
        value['state'] = state
        expected = {'state': state, 'completed_jobs': 4, 'verified_exchanges': 1604,
                    'verified_field_macs': 192862767104}
        cases.append(test(args.binary, state, [value] * 3, expected))
    large = copy.deepcopy(GOOD)
    large['telemetry']['maskedMacs'] = 9007199254740993
    cases.append(test(args.binary, 'exact-integer-above-double', [large] * 3,
        {'state': 'ready', 'completed_jobs': 4, 'verified_exchanges': 1604,
         'verified_field_macs': 9007199254740993}))
    malformed = [('bad-json', b'{'), ('wrong-root', []), ('trailing-json', json.dumps(GOOD).encode() + b'{}'),
                 ('oversize-frame', 'oversize-frame'), ('zero-frame', 'zero-frame'),
                 ('truncated-frame', 'truncated-frame')]
    for key, value in [('ok', False), ('ok', 1), ('schema', 'unknown'), ('state', 'unknown'),
                       ('state', 'ready\0evil'), ('gpuRoute', 'raw'), ('completed', -1),
                       ('completed', True), ('completed', 1.5), ('completed', 1 << 63)]:
        invalid = copy.deepcopy(GOOD)
        invalid[key] = value
        malformed.append((f'invalid-{key}-{len(malformed)}', invalid))
    for value in (-1, True, '10', 1.5, 1 << 64):
        invalid = copy.deepcopy(GOOD)
        invalid['telemetry']['maskedMacs'] = value
        malformed.append((f'invalid-counter-{len(malformed)}', invalid))
    for name, invalid in malformed:
        cases.append(test(args.binary, name, [invalid], 'absent'))
    cases.append(test(args.binary, 'wrong-peer-uid', [GOOD], 'absent', uid=0))
    cases.append(test(args.binary, 'bounded-stalled-peer', [GOOD], 'absent', slow=True))
    cases.append(test(args.binary, 'stale-snapshot-cleared', [GOOD, GOOD, b'{'], None))
    args.output.write_text(json.dumps({'schema': 'shield-nvtop-tests/1', 'cases': cases}, indent=2) + '\n')
    print(f'{len(cases)} monitor fixtures passed.', flush=True)


if __name__ == '__main__':
    main()
