#!/usr/bin/env python3
"""Real nft regression; run only in an isolated network-none container.

No model, GPU or credentials. Both directions must belong to desktop UID 1000
for a desktop-initiated private helper connection. Control routes stay fixed.
"""
import json
import multiprocessing as mp
import os
from pathlib import Path
import socket
import subprocess
import sys


def identity(uid):
    os.setgroups([])
    os.setgid(uid)
    os.setuid(uid)


def server(pipe, uid, address, port):
    identity(uid)
    try:
        with socket.socket() as listener:
            listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            listener.bind((address, port))
            listener.listen()
            listener.settimeout(1.5)
            pipe.send(listener.getsockname()[1])
            with listener.accept()[0] as peer:
                peer.settimeout(1)
                if peer.recv(32) == b'public-fixture':
                    peer.sendall(b'public-reply')
    except OSError:
        pass
    finally:
        pipe.close()


def client(pipe, uid, address, port):
    identity(uid)
    try:
        with socket.socket() as sock:
            sock.settimeout(0.6)
            sock.bind(('127.0.0.1', 0))
            sock.connect((address, port))
            sock.sendall(b'public-fixture')
            result = sock.recv(32) == b'public-reply'
    except OSError:
        result = False
    pipe.send(result)
    pipe.close()


def exchange(server_uid, client_uid, address='127.0.0.1', port=0):
    parent, child = mp.Pipe(duplex=False)
    s = mp.Process(target=server, args=(child, server_uid, address, port))
    s.start()
    child.close()
    assert parent.poll(3), 'listener failed'
    port = parent.recv()
    parent.close()
    parent, child = mp.Pipe(duplex=False)
    c = mp.Process(target=client, args=(child, client_uid, address, port))
    c.start()
    child.close()
    assert parent.poll(3), 'client failed'
    result = parent.recv()
    parent.close()
    for process in (c, s):
        process.join(3)
        if process.is_alive():
            process.terminate()
            process.join()
            raise AssertionError('probe did not finish')
        assert process.exitcode == 0
    return result


def main():
    assert os.getuid() == 0
    assert set(os.listdir('/sys/class/net')) == {'lo'}, 'requires network-none namespace'
    rules = Path(sys.argv[1]).read_text()
    expected_desktop = sys.argv[2] == 'fixed'
    assert sys.argv[2] in ('baseline', 'fixed')
    subprocess.run(['nft', '-f', '-'], input=rules, text=True, check=True)
    cases = {
        'desktopOwn': (1000, 1000, '127.0.0.1', 0, expected_desktop),
        'privateService': (1001, 1001, '127.0.0.3', 0, True),
        'desktopToPrivate': (1001, 1000, '127.0.0.3', 0, False),
        'spoofPrivate': (1000, 1001, '127.0.0.3', 0, False),
        'frontToController': (5001, 1053577, '127.0.0.1', 8000, True),
        'controllerToGateway': (1002, 5001, '127.0.0.1', 19080, True),
        'gatewayToInner': (1003, 1002, '127.0.0.1', 18080, True),
        'innerToDisplay': (1000, 1003, '127.0.0.1', 5900, True),
        'desktopToController': (5001, 1000, '127.0.0.1', 8000, False),
        'desktopToGateway': (1002, 1000, '127.0.0.1', 19080, False),
        'desktopToInner': (1003, 1000, '127.0.0.1', 18080, False),
        'desktopToWebsocket': (1003, 1000, '127.0.0.1', 6080, False),
        'spoofController': (1000, 1053577, '127.0.0.1', 8000, False),
    }
    for uid in (0, 1001, 1002, 1003, 5001, 1053577):
        cases[f'desktopToOtherUID{uid}'] = (uid, 1000, '127.0.0.1', 0, False)
    checks = {}
    for name, (server_uid, client_uid, address, port, want) in cases.items():
        got = exchange(server_uid, client_uid, address, port)
        checks[name] = got
        assert got == want, (name, got, want)
    print(json.dumps({'passed': True, 'mode': sys.argv[2], 'checks': checks}))


if __name__ == '__main__':
    main()
