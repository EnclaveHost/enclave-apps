#!/usr/bin/python3
"""Boot-time checks run with the desktop uid and inherited sandbox."""
import ctypes
import errno
import glob
import json
import os
from pathlib import Path
import socket
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent))
from service import send, receive

assert os.getuid() == 1000, 'wrong boundary-test identity'
devices = glob.glob('/dev/nvidia*') + glob.glob('/dev/dri/*') + glob.glob('/dev/kfd')
assert not devices, 'raw GPU device present'
try:
    with socket.socket(socket.AF_VSOCK, socket.SOCK_STREAM):
        raise AssertionError('direct VSOCK allowed')
except PermissionError:
    pass
libc = ctypes.CDLL(None, use_errno=True)
assert libc.prctl(39, 0, 0, 0, 0) == 1, 'no_new_privs missing'
assert libc.syscall(425, 1, 0) == -1 and ctypes.get_errno() == errno.EPERM, 'io_uring allowed'
with socket.socket() as sock:
    sock.settimeout(2)
    try:
        sock.connect(('10.0.2.2', 9741))
    except OSError as error:
        assert error.errno in (errno.EPERM, errno.EACCES, errno.ECONNREFUSED,
                               errno.EHOSTUNREACH, errno.ENETUNREACH), 'inconclusive worker isolation'
    else:
        raise AssertionError('desktop can reach raw worker')
with socket.socket(socket.AF_UNIX) as sock:
    sock.settimeout(5)
    sock.connect('/run/shield/compute.sock')
    send(sock, {'op': 'status'})
    assert receive(sock, 65536)['state'] == 'ready', 'local service unavailable'
print(json.dumps({'uid': os.getuid(), 'rawGpuDevices': devices, 'vsockDenied': True,
                  'ioUringDenied': True, 'privilegeEscalationDisabled': True,
                  'directWorkerDenied': True, 'localServiceAvailable': True}))
