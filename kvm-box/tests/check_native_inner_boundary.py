#!/usr/bin/env python3
"""Production inner binary, role policy, RO assets and nft; backend fixtures.

Fresh mapped user/mount/net/PID namespaces required; never run as host root.
Arguments: BUILD ASSETS OUT. BUILD contains inner and inner-role-fixture.
No real desktop, models, GPU, owner credential, Sunshine or admission is used.
"""
import base64
import hashlib
import http.client
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import select
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

BUILD, ASSETS, OUT = [Path(sys.argv.pop(1)).resolve() for _ in range(3)]
APP = Path(__file__).resolve().parents[1]


def exact(sock, n):
    data = bytearray()
    while len(data) < n:
        part = sock.recv(n-len(data))
        if not part:
            raise EOFError('truncated fixture frame')
        data.extend(part)
    return bytes(data)


class InnerBoundary(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        mapping = Path('/proc/self/uid_map').read_text().splitlines()[0].split()
        parent = os.environ.get('KVMBOX_TEST_PARENT_NETNS')
        if (os.getuid() != 0 or os.getpid() != 1 or mapping[0] != '0'
                or mapping[1] == '0' or mapping[2] != '1'):
            raise RuntimeError('mapped namespace root/PID 1 required; never host root')
        if mapping[1] == '0' or not parent or os.readlink('/proc/self/ns/net') == parent:
            raise RuntimeError('fresh mapped user and network namespaces required')
        OUT.mkdir(parents=True, exist_ok=True)
        cls.processes = []
        cls.children = []
        cls.mounts = []
        temp = tempfile.TemporaryDirectory(prefix='inner-boundary-', dir=OUT)
        cls.temporary = temp
        cls.root = Path(temp.name); cls.root.chmod(0o755)
        cls.addClassCleanup(cls.shutdown)
        subprocess.run(['mount', '--make-rprivate', '/'], check=True)
        for name in ['dev', 'run/shield', 'usr/share/kvm-box']:
            (cls.root/name).mkdir(parents=True, exist_ok=True)
        cls.static = cls.root/'usr/share/kvm-box/native-ui'
        shutil.copytree(ASSETS, cls.static)
        # Source staging belongs to the caller; copying here establishes the
        # intended image owner inside this disposable mapped namespace.
        for p in [cls.static, *cls.static.rglob('*')]:
            os.chown(p, 0, 0)
        subprocess.run(['mount', '--bind', str(cls.static), str(cls.static)], check=True)
        cls.mounts.append(cls.static)
        subprocess.run(['mount', '-o', 'remount,bind,ro,nosuid,nodev', str(cls.static)], check=True)
        (cls.root/'dev/null').touch()
        subprocess.run(['mount', '--bind', '/dev/null', str(cls.root/'dev/null')], check=True)
        cls.mounts.append(cls.root/'dev/null')
        for name in ['inner', 'inner-role-fixture']:
            shutil.copyfile(BUILD/name, cls.root/name); (cls.root/name).chmod(0o555)
        os.chown(cls.root/'run/shield', 1001, 1000); (cls.root/'run/shield').chmod(0o750)
        subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
        spec = importlib.util.spec_from_file_location('policy', APP/'guest/native/network_policy.py')
        policy = importlib.util.module_from_spec(spec); spec.loader.exec_module(policy)
        rules = policy.policy(7501)
        subprocess.run(['nft', '-f', '-'], input=rules, text=True, check=True)
        (OUT/'policy.nft').write_text(rules)
        cls.spawn_fixture(1001, cls.compute_server)
        cls.spawn_fixture(1003, cls.websocket_server)
        cls.spawn_fixture(1000, cls.tcp_server)
        cls.spawn_fixture(1000, cls.udp_server)
        def prepare():
            os.chroot(cls.root); os.chdir('/')
        cls.inner = subprocess.Popen(['/inner-role-fixture'], preexec_fn=prepare,
                                     env={}, stdin=subprocess.DEVNULL,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        cls.processes.append(cls.inner)
        deadline = time.monotonic()+5
        while time.monotonic() < deadline:
            if cls.inner.poll() is not None:
                raise AssertionError(f'inner exited {cls.inner.returncode}')
            try:
                if cls.request('/vnc.html')[0] == 200:
                    return
            except (OSError, http.client.HTTPException):
                pass
            time.sleep(.025)
        raise AssertionError('inner failed readiness fixture')

    @classmethod
    def shutdown(cls):
        for proc in cls.processes:
            if proc.poll() is None:
                proc.terminate()
                try: proc.wait(timeout=3)
                except subprocess.TimeoutExpired: proc.kill(); proc.wait()
        for pid in cls.children:
            try: os.kill(pid, signal.SIGKILL)
            except ProcessLookupError: pass
            try: os.waitpid(pid, 0)
            except ChildProcessError: pass
        for path in reversed(cls.mounts):
            subprocess.run(['umount', str(path)], check=True)
        if hasattr(cls, 'static'):
            # Once unmounted, restore staging modes for TemporaryDirectory.
            for p in [cls.static, *cls.static.rglob('*')]:
                if p.is_dir(): p.chmod(0o755)
        cls.temporary.cleanup()

    @classmethod
    def spawn_fixture(cls, uid, server):
        readfd, writefd = os.pipe()
        pid = os.fork()
        if pid == 0:
            try:
                os.close(readfd)
                os.chroot(cls.root); os.chdir('/'); cls.root = Path('/')
                os.setgroups([]); os.setgid(1000 if uid == 1001 else uid); os.setuid(uid)
                server(writefd)
            except BaseException:
                os._exit(1)
            os._exit(0)
        cls.children.append(pid); os.close(writefd)
        # A failed fixture closes this pipe on process exit.
        if not select.select([readfd], [], [], 5)[0] or os.read(readfd, 1) != b'R':
            os.close(readfd); raise AssertionError('fixture startup failed')
        os.close(readfd)

    @classmethod
    def compute_server(cls, ready):
        s = socket.socket(socket.AF_UNIX)
        s.bind(str(cls.root/'run/shield/compute.sock'))
        os.chmod(cls.root/'run/shield/compute.sock', 0o660)
        s.listen(); os.write(ready, b'R'); os.close(ready)
        while True:
            c, _ = s.accept()
            with c:
                c.settimeout(3)
                _, uid, _ = struct.unpack('3i', c.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
                if uid != 1003: continue
                n, = struct.unpack('!I', exact(c, 4))
                if not 0 < n <= 65536: continue
                request = json.loads(exact(c, n))
                response = {'ok': True, 'state': 'ready', 'peerUid': uid, 'fixture': True}
                if request.get('op') != 'status':
                    response.update(text=request.get('prompt'), arithmetic=request.get('arithmetic'))
                body = json.dumps(response, separators=(',', ':')).encode()
                c.sendall(struct.pack('!I', len(body))+body)

    @staticmethod
    def listener(port, ready, kind=socket.SOCK_STREAM):
        s = socket.socket(socket.AF_INET, kind); s.bind(('127.0.0.1', port))
        if kind == socket.SOCK_STREAM: s.listen()
        os.write(ready, b'R'); os.close(ready)
        return s

    @classmethod
    def websocket_server(cls, ready):
        s = cls.listener(6080, ready)
        while True:
            c, _ = s.accept()
            with c:
                c.settimeout(3); head = bytearray()
                while not head.endswith(b'\r\n\r\n'): head.extend(exact(c, 1))
                headers = dict(line.split(': ', 1) for line in head.decode().split('\r\n')[1:-2])
                key = headers['Sec-WebSocket-Key']
                accept = base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
                c.sendall(('HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: '+accept+'\r\n\r\n').encode()+b'early')
                while data := c.recv(4096): c.sendall(data)
                c.sendall(b'half-close')

    @classmethod
    def tcp_server(cls, ready):
        s = cls.listener(48010, ready)
        while True:
            c, _ = s.accept()
            with c:
                c.settimeout(3)
                while data := c.recv(4096): c.sendall(data)
                c.sendall(b'half-close')

    @classmethod
    def udp_server(cls, ready):
        s = cls.listener(47998, ready, socket.SOCK_DGRAM)
        while True:
            data, peer = s.recvfrom(65535); s.sendto(data, peer)

    @staticmethod
    def connection(uid=1002):
        old = os.geteuid()
        try:
            os.seteuid(uid)
            return socket.create_connection(('127.0.0.1', 18080), .5)
        finally: os.seteuid(old)

    @classmethod
    def request(cls, path, method='GET', body=None, extra=None):
        c = http.client.HTTPConnection('127.0.0.1', 18080, timeout=3)
        headers = {'Connection':'close'}; headers.update(extra or {})
        try:
            c.sock = cls.connection(); c.request(method, path, body, headers)
            r = c.getresponse(); return r.status, dict(r.headers), r.read()
        finally: c.close()

    def test_01_role_and_read_only_assets(self):
        status = dict(line.split(':', 1) for line in Path(f'/proc/{self.inner.pid}/status').read_text().splitlines() if ':' in line)
        self.assertEqual(status['Uid'].split(), ['1003']*4)
        self.assertEqual(status['NoNewPrivs'].strip(), '1')
        self.assertEqual(status['Seccomp'].strip(), '2')
        for field in ['CapInh','CapPrm','CapEff','CapBnd','CapAmb']: self.assertEqual(int(status[field],16), 0)
        for route, file in [('/vnc.html','novnc/vnc.html'),('/compute/','native.html'),('/compute/native/','native.html')]:
            code, headers, data = self.request(route)
            self.assertEqual(code, 200); self.assertEqual(data, (ASSETS/file).read_bytes())
            self.assertEqual(headers['X-Content-Type-Options'], 'nosniff')
            self.assertEqual(headers['X-Frame-Options'], 'SAMEORIGIN')
        for suffix, mime in [('mp3','audio/mpeg'),('oga','audio/ogg')]:
            code, headers, data = self.request('/app/sounds/bell.'+suffix)
            self.assertEqual(code,200); self.assertEqual(headers['Content-Type'],mime)
            self.assertEqual(data,(ASSETS/('novnc/app/sounds/bell.'+suffix)).read_bytes())
        self.assertEqual(self.request('/app/../native.html')[0], 400)
        self.assertEqual(self.request('/compute/', extra={'Authorization':'synthetic'})[0], 400)
        # The actual production RO check must reject this root-owned tree if
        # its filesystem becomes writable, then recover when restored.
        try:
            subprocess.run(['mount','-o','remount,bind,rw',str(self.static)],check=True)
            self.assertEqual(self.request('/vnc.html')[0],503)
        finally:
            subprocess.run(['mount','-o','remount,bind,ro,nosuid,nodev',str(self.static)],check=True)
        self.assertEqual(self.request('/vnc.html')[0],200)

    def test_02_compute_peer_credentials_and_json(self):
        code, _, data = self.request('/compute/api/native')
        self.assertEqual(code, 200); self.assertEqual(json.loads(data)['peerUid'], 1003)
        request = {'op':'llama','prompt':'<script>synthetic</script>','mode':'cpu','arithmetic':'native-model-v1'}
        code, headers, data = self.request('/compute/native/compute','POST',json.dumps(request),{'Content-Type':'application/json'})
        self.assertEqual(code,200); self.assertEqual(headers['Content-Type'],'application/json')
        self.assertEqual(json.loads(data)['text'],request['prompt'])
        self.assertEqual(self.request('/compute/native/compute','POST','{"op":"llama","op":"sd"}',{'Content-Type':'application/json'})[0],400)
        path = self.root/'run/shield/compute.sock'
        try:
            path.chmod(0o666); self.assertEqual(self.request('/compute/api/native')[0],503)
        finally: path.chmod(0o660)

    def test_03_wrong_network_role_refused(self):
        for uid in [0,1000,1001,1003,7501]:
            with self.subTest(uid=uid):
                with self.assertRaises(OSError): self.connection(uid)

    def stream(self, route, protocol, extra=''):
        c = self.connection(); c.settimeout(3)
        c.sendall((f'GET {route} HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nConnection: Upgrade\r\nUpgrade: {protocol}\r\n'+extra+'\r\n').encode())
        head = bytearray()
        while not head.endswith(b'\r\n\r\n'): head.extend(exact(c,1))
        self.assertTrue(head.startswith(b'HTTP/1.1 101 '),head)
        return c

    def test_04_websocket_early_bytes_and_half_close(self):
        with self.stream('/websockify','websocket','Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: MDEyMzQ1Njc4OWFiY2RlZg==\r\n') as c:
            self.assertEqual(exact(c,5),b'early')
            c.sendall(b'fixture'); self.assertEqual(exact(c,7),b'fixture')
            c.shutdown(socket.SHUT_WR); self.assertEqual(exact(c,10),b'half-close')

    def test_05_moonlight_tcp_udp_wire(self):
        with self.stream('/api/moonlight/tunnel?port=48010&transport=tcp','kvm-box-tunnel') as c:
            c.sendall(b'tcp'); self.assertEqual(exact(c,3),b'tcp')
            c.shutdown(socket.SHUT_WR); self.assertEqual(exact(c,10),b'half-close')
        with self.stream('/api/moonlight/tunnel?transport=udp&port=47998','kvm-box-tunnel') as c:
            for payload in [b'one', b'two'*300]:
                frame = struct.pack('!H',len(payload))+payload
                c.sendall(frame); self.assertEqual(exact(c,len(frame)),frame)
            c.sendall(b'\0\0'); self.assertEqual(c.recv(1), b'')


if __name__ == '__main__':
    unittest.main(verbosity=2)
