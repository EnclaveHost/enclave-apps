#!/usr/bin/env python3
"""Real Linux namespace/pidfd/Unix-credential tests, no desktop or host devices.
Run under unshare --map-root-user --map-auto --mount --pid --fork --mount-proc.
Arguments: test supervisor, fixture nsinit, output directory.
Production supervisor is separately built without NATIVE_SUPERVISOR_TEST.
"""
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time
import unittest

SUPERVISOR, FIXTURE, OUTPUT = map(lambda x: Path(x).resolve(), sys.argv[1:4])
del sys.argv[1:4]
OUTPUT.mkdir(parents=True, exist_ok=True)

class SupervisorChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        uid_map = Path('/proc/self/uid_map').read_text().splitlines()[0].split()
        if os.getuid() != 0 or uid_map[0] != '0' or uid_map[1] == '0' or uid_map[2] != '1' or os.getpid() != 1:
            raise RuntimeError('Requires fresh user/PID/mount namespaces; never run as host root')
        cls.directory = tempfile.TemporaryDirectory(prefix='supervisor-', dir=OUTPUT)
        cls.addClassCleanup(cls.directory.cleanup)
        cls.root = Path(cls.directory.name)
        cls.root.chmod(0o755)
        subprocess.run(['mount', '--bind', str(cls.root), str(cls.root)], check=True)
        cls.addClassCleanup(subprocess.run, ['umount', str(cls.root)], check=True)
        for directory in ['dev', 'run/native-desktop', 'plat/native-desktop']:
            (cls.root / directory).mkdir(parents=True, exist_ok=True)
        (cls.root / 'run/native-desktop').chmod(0o750)
        os.chown(cls.root / 'run/native-desktop', 0, 1002)
        (cls.root / 'dev/null').touch()
        subprocess.run(['mount', '--bind', '/dev/null', str(cls.root / 'dev/null')], check=True)
        cls.addClassCleanup(subprocess.run, ['umount', str(cls.root / 'dev/null')], check=True)
        shutil.copyfile(SUPERVISOR, cls.root / 'supervisor')
        (cls.root / 'supervisor').chmod(0o555)
        shutil.copyfile(FIXTURE, cls.root / 'plat/native-desktop/nsinit')
        (cls.root / 'plat/native-desktop/nsinit').chmod(0o555)
        (cls.root / 'desktop.profile').write_text('KVM-BOX-MASKED/1\n')
        (cls.root / 'desktop.profile').chmod(0o444)
        cls.proc = None
        cls.addClassCleanup(cls.stop_supervisor)

    @classmethod
    def stop_supervisor(cls):
        if cls.proc and cls.proc.poll() is None:
            cls.proc.terminate()
            try:
                cls.proc.wait(timeout=4)
            except subprocess.TimeoutExpired:
                cls.proc.kill(); cls.proc.wait(timeout=4)
        cls.proc = None

    @classmethod
    def inside(cls):
        os.chroot(cls.root)
        os.chdir('/')

    def launch(self, mode='normal', inherited=False):
        self.stop_supervisor()
        (self.root / 'fixture-mode').write_text(mode)
        def prepare():
            self.inside()
            if inherited:
                signal.signal(signal.SIGCHLD, signal.SIG_IGN)
                signal.pthread_sigmask(signal.SIG_BLOCK, {signal.SIGTERM, signal.SIGINT, signal.SIGCHLD})
                for fd in range(3):
                    os.close(fd)
        self.__class__.proc = subprocess.Popen(['/supervisor', '4', '8192'], preexec_fn=prepare)
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                self.fail(f'supervisor exited {self.proc.returncode}')
            if (self.root / 'run/native-desktop/control.sock').exists():
                return
            time.sleep(.01)
        self.fail('control socket did not appear')

    def control(self, command=b'Q', uid=1002, delayed_extra=None):
        read, write = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(read)
            try:
                self.inside()
                os.setgroups([]); os.setgid(1002); os.setuid(uid)
                with socket.socket(socket.AF_UNIX) as s:
                    s.settimeout(2)
                    s.connect('/run/native-desktop/control.sock')
                    s.sendall(command)
                    if delayed_extra is not None:
                        time.sleep(.03); s.sendall(delayed_extra)
                    s.shutdown(socket.SHUT_WR)
                    raw = b''
                    while True:
                        chunk = s.recv(4096)
                        if not chunk:
                            break
                        raw += chunk
                result = {'raw': raw.hex()}
            except OSError as e:
                result = {'error': type(e).__name__, 'errno': e.errno}
            os.write(write, json.dumps(result).encode())
            os.close(write); os._exit(0)
        os.close(write)
        result = json.loads(os.read(read, 8192))
        os.close(read)
        _, status = os.waitpid(pid, 0)
        self.assertEqual(status, 0)
        if uid == 1002 and command == b'Q' and 'error' in result:
            self.fail(f'authorized status transport failed: {result}')
        if 'raw' not in result or not result['raw']:
            return None
        raw = bytes.fromhex(result['raw'])
        self.assertGreaterEqual(len(raw), 4)
        n, = struct.unpack('>I', raw[:4])
        self.assertEqual(n, len(raw) - 4)
        data = json.loads(raw[4:])
        self.assertEqual(set(data), {'status', 'pid', 'vcpus', 'memoryMiB'})
        self.assertEqual(data['vcpus'], 4)
        self.assertEqual(data['memoryMiB'], 8192)
        return data

    def wait_state(self, wanted, seconds=3):
        until = time.monotonic() + seconds
        while time.monotonic() < until:
            status = self.control()
            if status and status['status'] == wanted:
                return status
            time.sleep(.025)
        self.fail(f'state did not become {wanted}: {status}')

    def escaped_processes(self):
        found = []
        for p in Path('/proc').iterdir():
            if p.name.isdigit():
                try:
                    if (p / 'comm').read_text().strip() == 'kvm-ns-escape':
                        found.append(int(p.name))
                except (FileNotFoundError, PermissionError):
                    pass
        return found

    def assert_no_descendants(self):
        until = time.monotonic() + 3
        while time.monotonic() < until:
            if not self.escaped_processes():
                return
            time.sleep(.02)
        self.fail('escaped process survived namespace init teardown')

    def tearDown(self):
        self.stop_supervisor()
        self.assert_no_descendants()

    def test_01_start_stop_restart_kills_escaped_group(self):
        self.launch()
        self.assertEqual(self.control()['status'], 'stopped')
        self.assertEqual(self.control(b'B')['status'], 'starting')
        first = self.wait_state('running')
        self.assertTrue(self.escaped_processes())
        self.assertEqual(self.control(b'B')['pid'], first['pid'])
        self.assertEqual(self.control(b'H')['status'], 'stopping')
        self.wait_state('stopped')
        self.assert_no_descendants()
        self.control(b'B')
        second = self.wait_state('running')
        self.assertNotEqual(first['pid'], second['pid'])
        self.assertTrue(self.escaped_processes())

    def test_02_peer_uid_and_command_framing(self):
        self.launch()
        for uid in [0, 1000]:
            self.assertIsNone(self.control(b'B', uid=uid))
        for command, extra in [(b'', None), (b'X', None), (b'BB', None), (b'B', b'Q')]:
            self.assertIsNone(self.control(command, delayed_extra=extra))
        self.assertEqual(self.control()['status'], 'stopped')
        self.assertFalse(self.escaped_processes())

    def test_03_readiness_rejects_extra_bytes_and_child_failure(self):
        for mode in ['bad-ready', 'split-invalid', 'die']:
            with self.subTest(mode=mode):
                self.launch(mode)
                self.control(b'B')
                self.wait_state('failed')
                self.assert_no_descendants()
                self.stop_supervisor()

    def test_04_supervisor_death_kills_namespace(self):
        self.launch()
        self.control(b'B'); self.wait_state('running')
        self.assertTrue(self.escaped_processes())
        self.proc.kill(); self.proc.wait(timeout=3)
        self.assert_no_descendants()
        # A stale pathname is not permission to reuse another listener.
        stale = subprocess.run(['/supervisor', '4', '8192'], preexec_fn=self.inside, timeout=3)
        self.assertEqual(stale.returncode, 66)
        (self.root / 'run/native-desktop/control.sock').unlink()

    def test_05_profile_marker_and_entry_permissions(self):
        marker = self.root / 'desktop.profile'
        entry = self.root / 'plat/native-desktop/nsinit'
        for path in [marker, entry]:
            old = path.stat().st_mode & 0o777
            path.chmod(0o777)
            try:
                r = subprocess.run(['/supervisor', '4', '8192'], preexec_fn=self.inside, timeout=3)
                self.assertEqual(r.returncode, 64)
            finally:
                path.chmod(old)
        marker.chmod(0o644); marker.write_text('KVM-BOX-MASKED/1\nextra'); marker.chmod(0o444)
        try:
            r = subprocess.run(['/supervisor', '4', '8192'], preexec_fn=self.inside, timeout=3)
            self.assertEqual(r.returncode, 64)
        finally:
            marker.chmod(0o644); marker.write_text('KVM-BOX-MASKED/1\n'); marker.chmod(0o444)

    def test_06_inherited_signals_and_closed_stdio_are_reset(self):
        self.launch(inherited=True)
        self.control(b'B'); self.wait_state('running')
        self.control(b'H'); self.wait_state('stopped')
        self.control(b'B'); self.wait_state('running')
        self.proc.terminate()
        self.assertEqual(self.proc.wait(timeout=2), 0)
        self.assert_no_descendants()

    def test_07_child_mounts_do_not_propagate_to_supervisor(self):
        directory = self.root / 'mount-probe'
        directory.mkdir(exist_ok=True)
        subprocess.run(['mount', '--make-shared', str(self.root)], check=True)
        try:
            self.launch('mount-check')
            self.control(b'B'); self.wait_state('running')
            self.assertFalse((directory / 'child-only').exists())
            self.control(b'H'); self.wait_state('stopped')
            self.assertFalse((directory / 'child-only').exists())
        finally:
            subprocess.run(['mount', '--make-private', str(self.root)], check=True)

    def test_08_startup_deadline_kills_unready_namespace(self):
        self.launch('never-ready')
        started = time.monotonic()
        self.assertEqual(self.control(b'B')['status'], 'starting')
        self.wait_state('failed', seconds=125)
        elapsed = time.monotonic() - started
        self.assertGreaterEqual(elapsed, 119)
        self.assertLess(elapsed, 125)
        self.assert_no_descendants()
        self.launch()
        self.control(b'B'); self.wait_state('running')

    def test_09_stop_during_startup_kills_unready_namespace(self):
        self.launch('never-ready')
        self.control(b'B')
        until = time.monotonic() + 2
        while not self.escaped_processes() and time.monotonic() < until:
            time.sleep(.01)
        self.assertTrue(self.escaped_processes())
        self.control(b'H'); self.wait_state('stopped')
        self.assert_no_descendants()

if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(SupervisorChecks))
    receipt = {'schema': 'kvm-box-native-supervisor-check/1', 'testsRun': result.testsRun,
        'failures': len(result.failures), 'errors': len(result.errors),
        'supervisorTestBinarySha256': hashlib.sha256(SUPERVISOR.read_bytes()).hexdigest(),
        'fixtureSha256': hashlib.sha256(FIXTURE.read_bytes()).hexdigest(),
        'scope': 'Actual clone namespaces, pidfd lifecycle and Unix peer credentials with synthetic nsinit. Test build bypasses kernel-release check only. No desktop, GPU, network confinement or attestation qualification.'}
    (OUTPUT / 'supervisor-result.json').write_text(json.dumps(receipt, indent=2) + '\n')
    raise SystemExit(0 if result.wasSuccessful() else 1)
