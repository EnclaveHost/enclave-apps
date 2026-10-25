#!/usr/bin/python3 -I
"""Fixed readiness probes for the measured native desktop nsinit.

Installed root-owned and read-only at /usr/local/libexec/kvm-box/readiness.py
and run only by nsinit as `python3 -I readiness.py MODE` after the role drop
for MODE. There are no endpoint, path, environment or option inputs: each mode
has one fixed endpoint, one required UID and an absolute deadline. Exit 0 means
ready; any other status means not ready. Nothing is printed, so no service
data, model output or error detail reaches a log.

worker  (UID 1001): reserved local broker route reachability only. This is not
                    remote attestation or a GPU work check; no bytes are sent.
compute (UID 1003): framed {"op":"status"} to the masking service.
inner   (UID 1002): GET /compute/api/native and /vnc.html on 127.0.0.1:18080.
"""
import json
import os
import socket
import stat
import struct
import sys
import time

INNER_ADDRESS = ('127.0.0.1', 18080)
INNER_HOST = b'127.0.0.1:18080'
MAX_FRAME = 64 * 1024
MAX_HEADERS = 16 * 1024
MAX_JSON_BODY = 64 * 1024
MAX_PAGE_BODY = 512 * 1024


class NotReady(Exception):
    pass


def require(condition):
    if not condition:
        raise NotReady()


def remaining(deadline):
    left = deadline - time.monotonic()
    require(left > 0)
    return left


def pinned_directory(path, uid, gid, mode):
    """Opens each component without following symlinks; checks the leaf."""
    fd = os.open('/', os.O_PATH | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        for part in path.strip('/').split('/'):
            nxt = os.open(part, os.O_PATH | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC, dir_fd=fd)
            os.close(fd)
            fd = nxt
        st = os.fstat(fd)
        require(stat.S_ISDIR(st.st_mode) and st.st_uid == uid and st.st_gid == gid
                and stat.S_IMODE(st.st_mode) == mode)
        return fd
    except BaseException:
        os.close(fd)
        raise


def unix_connect(directory, name, uid, gid, mode, deadline):
    st = os.stat(name, dir_fd=directory, follow_symlinks=False)
    require(stat.S_ISSOCK(st.st_mode) and st.st_uid == uid and st.st_gid == gid
            and stat.S_IMODE(st.st_mode) == mode)
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM | socket.SOCK_CLOEXEC)
    try:
        s.settimeout(remaining(deadline))
        # Resolve through the pinned directory, not a second path walk.
        s.connect(f'/proc/self/fd/{directory}/{name}')
        return s
    except BaseException:
        s.close()
        raise


def peer_uid(s):
    _, uid, _ = struct.unpack('3i', s.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
    return uid


def exact(s, n, deadline):
    data = bytearray()
    while len(data) < n:
        s.settimeout(remaining(deadline))
        chunk = s.recv(n - len(data))
        require(chunk)
        data.extend(chunk)
    return bytes(data)


def no_duplicates(pairs):
    result = {}
    for key, value in pairs:
        require(key not in result)
        result[key] = value
    return result


def reject_constant(_):
    raise NotReady()


def strict_json(data):
    return json.loads(data.decode('utf-8'), object_pairs_hook=no_duplicates,
                      parse_constant=reject_constant)


def ready_status(data):
    value = strict_json(data)
    require(isinstance(value, dict) and value.get('ok') is True and value.get('state') == 'ready')


def worker():
    deadline = time.monotonic() + 2
    directory = pinned_directory('/run/shield-worker', 0, 1001, 0o750)
    try:
        with unix_connect(directory, 'gpu0', 0, 1001, 0o660, deadline) as s:
            require(peer_uid(s) == 0)
    finally:
        os.close(directory)


def compute():
    deadline = time.monotonic() + 3
    directory = pinned_directory('/run/shield', 1001, 1000, 0o750)
    try:
        with unix_connect(directory, 'compute.sock', 1001, 1000, 0o660, deadline) as s:
            require(peer_uid(s) == 1001)
            request = b'{"op":"status"}'
            s.settimeout(remaining(deadline))
            s.sendall(struct.pack('!I', len(request)) + request)
            s.shutdown(socket.SHUT_WR)
            size, = struct.unpack('!I', exact(s, 4, deadline))
            require(1 <= size <= MAX_FRAME)
            body = exact(s, size, deadline)
            s.settimeout(remaining(deadline))
            require(s.recv(1) == b'')
            ready_status(body)
    finally:
        os.close(directory)


def http_get(path, deadline, max_body):
    """One bounded HTTP/1.1 exchange; returns (media type, body)."""
    request = (b'GET ' + path + b' HTTP/1.1\r\nHost: ' + INNER_HOST +
               b'\r\nConnection: close\r\n\r\n')
    limit = MAX_HEADERS + max_body
    data = bytearray()
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM | socket.SOCK_CLOEXEC) as s:
        s.settimeout(remaining(deadline))
        s.connect(INNER_ADDRESS)
        s.settimeout(remaining(deadline))
        s.sendall(request)
        while True:
            s.settimeout(remaining(deadline))
            chunk = s.recv(65536)
            if not chunk:
                break
            data.extend(chunk)
            require(len(data) <= limit)
    end = data.find(b'\r\n\r\n')
    require(0 < end <= MAX_HEADERS)
    lines = bytes(data[:end]).split(b'\r\n')
    body = bytes(data[end + 4:])
    status = lines[0].split(b' ', 2)
    require(len(status) >= 2 and status[0] == b'HTTP/1.1' and status[1] == b'200')
    headers = {}
    for line in lines[1:]:
        require(line and line[:1] not in (b' ', b'\t') and b':' in line)
        name, value = line.split(b':', 1)
        name = name.strip().lower()
        require(name and name not in headers)
        headers[name] = value.strip()
    require(b'transfer-encoding' not in headers)
    length = headers.get(b'content-length')
    if length is not None:
        require(length.isdigit() and int(length) == len(body))
    require(0 < len(body) <= max_body)
    media = headers.get(b'content-type', b'').split(b';', 1)[0].strip().lower()
    return media, body


def inner():
    deadline = time.monotonic() + 3
    media, body = http_get(b'/compute/api/native', deadline, MAX_JSON_BODY)
    require(media == b'application/json')
    ready_status(body)
    media, body = http_get(b'/vnc.html', deadline, MAX_PAGE_BODY)
    require(media == b'text/html')


PROBES = {'worker': (1001, worker), 'compute': (1003, compute), 'inner': (1002, inner)}


def main(argv):
    if len(argv) != 2 or argv[1] not in PROBES or not sys.flags.isolated:
        return 64
    uid, probe = PROBES[argv[1]]
    if os.getuid() != uid or os.geteuid() != uid:
        return 65
    try:
        probe()
    except Exception:
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main(sys.argv))
