#!/usr/bin/env python3
"""Production Wasip2 + Go gateway + C supervisor, with a fixture namespace init.

Run in fresh user/mount/PID/network namespaces, mapped subordinate UIDs:
KVMBOX_TEST_PARENT_NETNS=$(readlink /proc/self/ns/net) unshare --map-root-user
  --map-users=1:100000:65535 --map-users=1056077:165535:1 \
  --map-groups=1:100000:65535 --map-groups=1056077:165535:1 \
  --mount --net --pid --fork --mount-proc python3 SCRIPT BUILD COMPONENT OUT
The outer subordinate UID/GID ranges above are this host's example; adapt them.
BUILD contains supervisor-test, nsinit-fixture, gateway, gateway-role-fixture.
No real desktop/GPU.
"""
import base64
import hashlib
import http.client
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socket
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BUILD, COMPONENT, OUT = [Path(sys.argv.pop(1)).resolve() for _ in range(3)]
SOURCE = Path(__file__).resolve().parents[1]
PASSWORD = 'synthetic-control-chain-owner'
AUTH = 'Basic ' + base64.b64encode(('owner:' + PASSWORD).encode()).decode()

def front_connection():
    # Test driver impersonates only the measured front's socket UID. The actual
    # controller/gateway run in separate processes under their production UIDs.
    original = os.geteuid()
    try:
        os.seteuid(7501 + 1048576)
        return socket.create_connection(('127.0.0.1', 8000), 3)
    finally:
        os.seteuid(original)


class InnerFixture(http.server.BaseHTTPRequestHandler):
    protocol_version = 'HTTP/1.1'

    def log_message(self, *args):
        pass

    def do_GET(self):
        upgrade = self.headers.get('Upgrade')
        if upgrade:
            self.send_response(101)
            self.send_header('Connection', 'Upgrade')
            self.send_header('Upgrade', upgrade)
            if upgrade == 'websocket':
                key = self.headers['Sec-WebSocket-Key']
                accept = base64.b64encode(hashlib.sha1((key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest()).decode()
                self.send_header('Sec-WebSocket-Accept', accept)
            self.end_headers()
            self.connection.settimeout(5)
            while True:
                data = self.connection.recv(4096)
                if not data:
                    self.connection.sendall(b'fixture-half-close')
                    self.close_connection = True
                    return
                self.connection.sendall(data)
        else:
            body = json.dumps({'path': self.path, 'headers': dict(self.headers)}).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.send_header('Content-Type', 'application/json')
            self.end_headers()
            self.wfile.write(body)


class ControlChain(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        mapping = Path('/proc/self/uid_map').read_text().splitlines()[0].split()
        parent = os.environ.get('KVMBOX_TEST_PARENT_NETNS')
        if (os.getuid() != 0 or os.getpid() != 1 or mapping[0] != '0'
                or mapping[1] == '0' or mapping[2] != '1' or not parent
                or os.readlink('/proc/self/ns/net') == parent):
            raise RuntimeError('Fresh mapped user/mount/PID/network namespaces required; never host root')
        OUT.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(prefix='control-chain-', dir=OUT)
        cls.addClassCleanup(temporary.cleanup)
        cls.root = Path(temporary.name)
        cls.root.chmod(0o755)
        subprocess.run(['mount', '--bind', str(cls.root), str(cls.root)], check=True)
        cls.addClassCleanup(subprocess.run, ['umount', str(cls.root)], check=True)
        for directory in ['dev', 'run/native-desktop', 'plat/native-desktop']:
            (cls.root / directory).mkdir(parents=True, exist_ok=True)
        control = cls.root / 'run/native-desktop'
        os.chown(control, 0, 1002); control.chmod(0o750)
        (cls.root / 'dev/null').touch()
        subprocess.run(['mount', '--bind', '/dev/null', str(cls.root / 'dev/null')], check=True)
        cls.addClassCleanup(subprocess.run, ['umount', str(cls.root / 'dev/null')], check=True)
        for name, target in [('supervisor-test', 'supervisor'), ('gateway', 'gateway'),
                             ('gateway-role-fixture', 'gateway-role-fixture'),
                             ('nsinit-fixture', 'plat/native-desktop/nsinit')]:
            shutil.copyfile(BUILD / name, cls.root / target)
            (cls.root / target).chmod(0o555)
        (cls.root / 'desktop.profile').write_text('KVM-BOX-MASKED/1\n')
        (cls.root / 'desktop.profile').chmod(0o444)
        (cls.root / 'fixture-mode').write_text('normal')
        subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
        spec = importlib.util.spec_from_file_location('policy', SOURCE / 'guest/native/network_policy.py')
        module = importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
        policy = module.policy(7501)
        subprocess.run(['nft', '-f', '-'], input=policy, text=True, check=True)
        (OUT / 'chain-policy.nft').write_text(policy)
        cls.processes = []
        cls.addClassCleanup(cls.shutdown)
        cls.launch(['/supervisor', '4', '8192'], 0, chroot=True)
        cls.until(lambda: (control / 'control.sock').is_socket())
        cls.launch(['/gateway-role-fixture'], 0, chroot=True)
        # An inherited component fd avoids granting subordinate UIDs access to
        # the user's workspace. Only the synthetic owner secret is inherited.
        with COMPONENT.open('rb') as component:
            cls.launch(['/usr/bin/wasmtime', 'run', '-C', 'cache=n', '-S', 'inherit-network=y',
                        '--env', 'KVMBOX_OWNER_PASSWORD', f'/proc/self/fd/{component.fileno()}'],
                       7501, pass_fds=(component.fileno(),),
                       env={'PATH': '/usr/bin:/bin', 'KVMBOX_OWNER_PASSWORD': PASSWORD})
        cls.until(lambda: cls.request('/api/status')[0] == 200, seconds=20)

    @classmethod
    def launch(cls, argv, uid, chroot=False, pass_fds=(), env=None):
        def prepare():
            if chroot:
                os.chroot(cls.root)
            os.chdir('/')
            os.setgroups([]); os.setgid(uid); os.setuid(uid)
        log = (OUT / ('chain-' + Path(argv[0]).name + '.log')).open('wb')
        cls.addClassCleanup(log.close)
        proc = subprocess.Popen(argv, preexec_fn=prepare, pass_fds=pass_fds,
                                env=env or {'PATH': '/usr/bin:/bin'}, stdout=log, stderr=log)
        cls.processes.append(proc)
        return proc

    @classmethod
    def shutdown(cls):
        for proc in reversed(cls.processes):
            if proc.poll() is None:
                proc.terminate()
                try:
                    proc.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    proc.kill(); proc.wait(timeout=3)

    @classmethod
    def until(cls, predicate, seconds=5):
        deadline = time.monotonic() + seconds
        last = None
        while time.monotonic() < deadline:
            try:
                if predicate():
                    return
            except (OSError, http.client.HTTPException) as error:
                last = type(error).__name__
            for proc in cls.processes:
                if proc.poll() is not None:
                    raise AssertionError(f'child exited: {proc.args[0]} code {proc.returncode}')
            time.sleep(.025)
        raise AssertionError(f'condition timed out ({last})')

    @classmethod
    def request(cls, path, method='GET', authorized=True, action=True, extra=None):
        headers = {'Host': 'fixture.test', 'Connection': 'close'}
        if authorized:
            headers['Authorization'] = AUTH
        if method == 'POST' and action:
            headers['X-KVM-Box-Request'] = '1'
        headers.update(extra or {})
        c = http.client.HTTPConnection('127.0.0.1', 8000, timeout=3)
        try:
            c.sock = front_connection()
            c.request(method, path, headers=headers)
            r = c.getresponse()
            return r.status, r.read()
        finally:
            c.close()

    @classmethod
    def status(cls):
        code, body = cls.request('/api/status')
        if code != 200:
            raise AssertionError((code, body))
        return json.loads(body)

    def test_01_auth_and_actions(self):
        self.assertEqual(self.status()['status'], 'stopped')
        self.assertEqual(self.request('/api/start', 'POST', authorized=False)[0], 401)
        self.assertEqual(self.request('/api/start', 'POST', action=False)[0], 403)
        self.assertEqual(self.request('/desktop/v1/start', 'POST')[0], 404)
        self.assertEqual(self.status()['status'], 'stopped')
        self.assertEqual(self.request('/api/start', 'POST')[0], 200)
        self.until(lambda: self.status()['status'] == 'running')
        first = self.status()
        self.assertGreater(first['pid'], 1)
        self.assertEqual((first['vcpus'], first['memoryMiB']), (4, 8192))
        self.assertEqual(self.request('/api/start', 'POST')[0], 200)
        self.assertEqual(self.status()['pid'], first['pid'])
        self.assertEqual(self.request('/api/stop', 'POST')[0], 200)
        self.until(lambda: self.status()['status'] == 'stopped')
        self.assertEqual(self.status()['pid'], 0)
        self.assertEqual(self.request('/api/start', 'POST')[0], 200)
        self.until(lambda: self.status()['status'] == 'running')
        self.assertNotEqual(self.status()['pid'], first['pid'])
        self.assertEqual(self.request('/api/stop', 'POST')[0], 200)
        self.until(lambda: self.status()['status'] == 'stopped')

    def test_02_direct_tcp_bypass_refused(self):
        for port in [19080, 18080]:
            with self.subTest(port=port), self.assertRaises(OSError):
                socket.create_connection(('127.0.0.1', port), timeout=.5)

    def test_03_missing_inner_desktop_is_failure(self):
        self.assertEqual(self.request('/desktop/vnc.html')[0], 502)
        self.assertEqual(self.status()['status'], 'stopped')

    def inner_fixture(self):
        read, write = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(read)
            os.setgroups([]); os.setgid(1003); os.setuid(1003)
            try:
                server = http.server.ThreadingHTTPServer(('127.0.0.1', 18080), InnerFixture)
                os.write(write, b'R'); os.close(write)
                server.serve_forever()
            finally:
                os._exit(1)
        os.close(write)
        def cleanup():
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            os.waitpid(pid, 0)
        self.addCleanup(cleanup)
        self.assertEqual(os.read(read, 1), b'R')
        os.close(read)

    def test_04_proxy_paths_and_owner_credential_stripping(self):
        self.inner_fixture()
        code, body = self.request('/desktop/compute/status?test=1', extra={
            'X-Compute-Authorization': 'synthetic-compute-token', 'X-Forwarded-For': 'spoof',
            'X-Internal-Auth': 'spoof', 'Cookie': 'synthetic-cookie'})
        self.assertEqual(code, 200)
        record = json.loads(body)
        self.assertEqual(record['path'], '/compute/status?test=1')
        headers = {k.lower(): v for k, v in record['headers'].items()}
        self.assertEqual(headers['host'], '127.0.0.1:18080')
        self.assertEqual(headers['x-compute-authorization'], 'synthetic-compute-token')
        for key in ['authorization', 'x-forwarded-for', 'x-internal-auth', 'cookie']:
            self.assertNotIn(key, headers)

    def test_05_upgrades_and_half_close(self):
        self.inner_fixture()
        for protocol, path, headers in [
            ('websocket', '/desktop/websockify', 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n'),
            ('kvm-box-tunnel', '/desktop/api/moonlight/tunnel?transport=udp&port=47998', ''),
        ]:
            with self.subTest(protocol=protocol), front_connection() as s:
                s.settimeout(5)
                s.sendall((f'GET {path} HTTP/1.1\r\nHost: fixture.test\r\nAuthorization: {AUTH}\r\n'
                           f'Connection: Upgrade\r\nUpgrade: {protocol}\r\n{headers}\r\n').encode())
                response = b''
                while b'\r\n\r\n' not in response:
                    chunk = s.recv(4096)
                    self.assertTrue(chunk)
                    response += chunk
                self.assertTrue(response.startswith(b'HTTP/1.1 101'), response)
                payload = bytes(range(256)) * 128
                s.sendall(payload)
                result = response.split(b'\r\n\r\n', 1)[1]
                while len(result) < len(payload):
                    chunk = s.recv(65536)
                    self.assertTrue(chunk)
                    result += chunk
                self.assertEqual(result, payload)
                s.shutdown(socket.SHUT_WR)
                tail = b''
                while True:
                    chunk = s.recv(4096)
                    if not chunk:
                        break
                    tail += chunk
                self.assertEqual(tail, b'fixture-half-close')


if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(ControlChain))
    receipt = {'passed': result.wasSuccessful(), 'tests': result.testsRun,
               'scope': 'real Wasip2 controller, Go gateway under production role capability/seccomp policy, nft UID policy, C supervisor; fixture nsinit and exec adapter',
               'fullDesktop': False, 'gpu': False,
               'sha256': {p.name: hashlib.sha256(p.read_bytes()).hexdigest() for p in
                          [COMPONENT, BUILD / 'gateway', BUILD / 'supervisor-test', BUILD / 'nsinit-fixture', BUILD / 'gateway-role-fixture']}}
    (OUT / 'control-chain-result.json').write_text(json.dumps(receipt, indent=2) + '\n')
    sys.exit(0 if result.wasSuccessful() else 1)
