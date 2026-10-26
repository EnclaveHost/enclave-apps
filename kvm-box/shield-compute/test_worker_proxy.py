#!/usr/bin/env python3
"""Opt-in fault proxy for the dedicated KVM Box test worker only.

Run the test worker on loopback 9742 and this proxy on loopback 9741. Never use
production workers. Reports contain counters/fingerprints, never packet bytes.
The proxy is deliberately untrusted and does not implement Shield verification.
"""
import argparse
import hashlib
import json
from pathlib import Path
import signal
import socket
import struct
import time


def receive(sock, length):
    result = bytearray()
    while len(result) < length:
        part = sock.recv(length - len(result))
        if not part:
            raise EOFError
        result.extend(part)
    return bytes(result)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mode', required=True, choices=('pass', 'corrupt', 'replay', 'disconnect'))
    parser.add_argument('--report', type=Path, required=True)
    parser.add_argument('--seconds', type=int, default=300)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 1800:
        parser.error('--seconds must be between 1 and 1800')
    report = {'schema': 'shield-worker-fault-test/1', 'mode': args.mode,
              'connections': 0, 'matrixReplies': 0, 'modifiedReplies': 0,
              'disconnectedRequests': 0, 'commands': [], 'maskedRequestHashes': [],
              'errors': [], 'state': 'starting'}
    stop = False
    saved_reply = None

    def save():
        temporary = args.report.with_suffix('.tmp')
        temporary.write_text(json.dumps(report, indent=2) + '\n')
        temporary.replace(args.report)

    def terminate(_signum, _frame):
        nonlocal stop
        stop = True

    signal.signal(signal.SIGTERM, terminate)
    signal.signal(signal.SIGINT, terminate)
    deadline = time.monotonic() + args.seconds
    with socket.socket() as listener:
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(('127.0.0.1', 9741))
        listener.listen(4)
        listener.settimeout(.25)
        report['state'] = 'listening'
        save()
        while not stop and time.monotonic() < deadline:
            try:
                guest, _ = listener.accept()
            except socket.timeout:
                continue
            report['connections'] += 1
            save()
            try:
                with guest, socket.create_connection(('127.0.0.1', 9742), 15) as worker:
                    guest.settimeout(15)
                    worker.settimeout(15)
                    while not stop:
                        header = receive(guest, 9)
                        command, size = struct.unpack('<BQ', header)
                        if size > 16 * 1024 * 1024:
                            raise ValueError('frame limit')
                        payload = receive(guest, size)
                        report['commands'].append(command)
                        matrix = command in (12, 13)
                        if matrix:
                            report['maskedRequestHashes'].append(hashlib.sha256(payload[12:]).hexdigest())
                            if args.mode == 'disconnect':
                                report['disconnectedRequests'] += 1
                                save()
                                break
                        worker.sendall(header + payload)
                        reply_header = receive(worker, 9)
                        status, size = struct.unpack('<BQ', reply_header)
                        if size > 16 * 1024 * 1024:
                            raise ValueError('reply limit')
                        reply = receive(worker, size)
                        if matrix and status == 0 and reply:
                            report['matrixReplies'] += 1
                            if args.mode == 'corrupt':
                                reply = bytes([reply[0] ^ 1]) + reply[1:]
                                report['modifiedReplies'] += 1
                            elif args.mode == 'replay':
                                if saved_reply is None:
                                    saved_reply = reply
                                else:
                                    if len(saved_reply) != len(reply):
                                        raise ValueError('replay fixture shape changed')
                                    reply = saved_reply
                                    report['modifiedReplies'] += 1
                        guest.sendall(reply_header + reply)
                        save()
            except EOFError:
                pass
            except (OSError, ValueError) as error:
                report['errors'].append(type(error).__name__)
            save()
        report['state'] = 'stopped'
        save()


if __name__ == '__main__':
    main()
