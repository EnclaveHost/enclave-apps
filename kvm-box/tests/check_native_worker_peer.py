#!/usr/bin/env python3
"""Exercise the actual rebuilt core against real Unix peer credentials.

Run in fresh mapped user/mount/PID namespaces, never as host root.
Argument: path to rebuilt libshield-core.so or libggml-shielded.so.
For a ggml backend, pass --ggml-runtime PATH_TO_LIBGGML_SO to reproduce the
engine's global symbol provider. This is a test-only loader option.
The servers are byte-empty fixtures, not workers or attestation evidence.
"""
import ctypes as C
import json
import os
from pathlib import Path
import resource
import select
import signal
import socket
import struct
import sys
import tempfile
import unittest

LIBRARY = Path(sys.argv.pop(1)).resolve()
GGML_RUNTIME = None
if len(sys.argv) > 1 and sys.argv[1] == '--ggml-runtime':
    sys.argv.pop(1)
    GGML_RUNTIME = Path(sys.argv.pop(1)).resolve()


def receive(fd, limit=4096):
    if not select.select([fd], [], [], 5)[0]:
        raise AssertionError('fixture timed out')
    value = os.read(fd, limit)
    if not value:
        raise AssertionError('fixture exited without result')
    return value


def fd_count():
    count = 0
    for fd in range(128):
        try: os.fstat(fd); count += 1
        except OSError: pass
    return count


class WorkerPeer(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        mapping = Path('/proc/self/uid_map').read_text().splitlines()[0].split()
        if (os.getuid() != 0 or os.getpid() != 1 or mapping[0] != '0'
                or mapping[1] == '0' or mapping[2] != '1'):
            raise RuntimeError('fresh mapped user/PID namespaces required; never host root')
        if GGML_RUNTIME:
            cls.ggml_runtime = C.CDLL(str(GGML_RUNTIME),mode=C.RTLD_GLOBAL)
        cls.lib = C.CDLL(str(LIBRARY))
        cls.lib.sh_pipe_open.argtypes = [C.c_char_p, C.c_int, C.POINTER(C.c_int)]
        cls.lib.sh_pipe_open.restype = C.c_void_p
        cls.lib.sh_pipe_close.argtypes = [C.c_void_p]
        cls.lib.sh_pipe_close.restype = None

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='worker-peer-')
        self.root = Path(self.temp.name); self.root.chmod(0o755)
        (self.root/'run/shield-worker').mkdir(parents=True)
        (self.root/'run/shield-worker').chmod(0o777)
        self.children = []; self.fds = []

    def tearDown(self):
        for pid in self.children:
            try: os.kill(pid,signal.SIGKILL)
            except ProcessLookupError: pass
            try: os.waitpid(pid,0)
            except ChildProcessError: pass
        for fd in self.fds:
            try: os.close(fd)
            except OSError: pass
        self.temp.cleanup()

    def pipe(self):
        pair = os.pipe(); self.fds.extend(pair); return pair

    def exchange(self, server_uid, count=1):
        ready_r, ready_w = self.pipe()
        result_r, result_w = self.pipe()
        child_r, child_w = self.pipe()
        path = '/run/shield-worker/gpu0'
        server = os.fork()
        if server == 0:
            try:
                os.chroot(self.root); os.chdir('/')
                os.setgroups([]); os.setgid(server_uid); os.setuid(server_uid)
                s = socket.socket(socket.AF_UNIX); s.bind(path); s.listen(8)
                os.write(ready_w,b'R')
                seen = []
                for _ in range(count):
                    c, _ = s.accept()
                    with c:
                        c.settimeout(5)
                        _, uid, _ = struct.unpack('3i', c.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
                        seen.append(uid)
                        # Neither successful open/close nor refusal sends data.
                        if c.recv(1) != b'': raise AssertionError('unexpected protocol data')
                os.write(result_w,json.dumps({'peers':seen,'protocolBytes':0}).encode())
                os._exit(0)
            except BaseException:
                os._exit(1)
        self.children.append(server)
        self.assertEqual(receive(ready_r),b'R')
        sockfile = self.root/path[1:]
        # Make even an impostor's socket metadata look like the real broker.
        os.chown(sockfile,0,1001); sockfile.chmod(0o660)
        os.chown(sockfile.parent,0,1001); sockfile.parent.chmod(0o750)
        client = os.fork()
        if client == 0:
            try:
                os.chroot(self.root); os.chdir('/')
                os.setgroups([1001]); os.setgid(1000); os.setuid(1001)
                resource.setrlimit(resource.RLIMIT_NOFILE,(128,128))
                before = fd_count(); results = []
                for _ in range(count):
                    error = C.c_int(999)
                    p = self.lib.sh_pipe_open(('unix:'+path).encode(),19595,C.byref(error))
                    results.append([bool(p),error.value])
                    if p: self.lib.sh_pipe_close(p)
                os.write(child_w,json.dumps({'results':results,'fdDelta':fd_count()-before}).encode())
                os._exit(0)
            except BaseException:
                os._exit(1)
        self.children.append(client)
        result = json.loads(receive(child_r))
        observed = json.loads(receive(result_r))
        for pid in [server,client]:
            _, status = os.waitpid(pid,0); self.children.remove(pid)
            self.assertEqual(os.waitstatus_to_exitcode(status),0)
        self.assertEqual(observed['peers'],[1001]*count)
        self.assertEqual(observed['protocolBytes'],0)
        self.assertEqual(result['fdDelta'],0)
        expected = [True,0] if server_uid == 0 else [False,-1]
        self.assertEqual(result['results'],[expected]*count)

    def test_root_broker_accepted(self): self.exchange(0)
    def test_desktop_impostor_refused_despite_root_socket_owner(self): self.exchange(1000)
    def test_compute_impostor_refused_despite_root_socket_owner(self): self.exchange(1001)
    def test_inner_ui_impostor_refused_despite_root_socket_owner(self): self.exchange(1003)
    def test_repeated_refusal_does_not_leak_descriptors(self): self.exchange(1000,100)

    def test_invalid_or_missing_unix_paths_refused(self):
        for host in [b'unix:',b'unix:relative',b'unix:/'+b'x'*120,
                     ('unix:'+str(self.root/'missing')).encode()]:
            with self.subTest(host=host):
                error = C.c_int(999)
                p = self.lib.sh_pipe_open(host,19595,C.byref(error))
                self.assertFalse(p); self.assertEqual(error.value,-1)


if __name__ == '__main__': unittest.main(verbosity=2)
