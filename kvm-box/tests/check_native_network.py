#!/usr/bin/env python3
"""Exercise actual nftables UID gates in fresh user/network/PID namespaces.
Map UIDs 0, 1000–1003, 7501, 65534 and front 1056077 in fresh namespaces.
See exported commands.json for this host's subordinate UID mapping.
"""
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
import unittest

OUT = Path(sys.argv.pop(1)).resolve()
OUT.mkdir(parents=True, exist_ok=True)
SOURCE = Path(__file__).resolve().parents[1] / 'guest/native/network_policy.py'
spec = importlib.util.spec_from_file_location('native_network_policy', SOURCE)
policy_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(policy_module)


class NetworkChecks(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        uid_map = Path('/proc/self/uid_map').read_text().splitlines()[0].split()
        if os.getuid() != 0 or os.getpid() != 1 or uid_map != ['0', str(os.stat(SOURCE).st_uid), '1']:
            # The source's host owner is mapped to 0, so use the namespace map
            # itself for the actual host-root refusal below instead.
            if os.getuid() != 0 or os.getpid() != 1 or uid_map[0] != '0' or uid_map[1] == '0' or uid_map[2] != '1':
                raise RuntimeError('Requires isolated user/PID/network namespaces; never host root')
        parent = os.environ.get('KVMBOX_TEST_PARENT_NETNS')
        if not parent or os.readlink('/proc/self/ns/net') == parent:
            raise RuntimeError('Fresh network namespace required')
        subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
        text = policy_module.policy(7501)
        (OUT / 'policy.nft').write_text(text)
        subprocess.run(['nft', '-f', '-'], input=text, text=True, check=True)
        cls.servers = []
        cls.addClassCleanup(cls.stop_servers)

    @classmethod
    def stop_servers(cls):
        for pid in cls.servers:
            try:
                os.kill(pid, signal.SIGKILL)
                os.waitpid(pid, 0)
            except ProcessLookupError:
                pass
        cls.servers = []

    def listener(self, uid, address, port, udp=False):
        read, write = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(read)
            try:
                os.setgroups([]); os.setgid(uid); os.setuid(uid)
                s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM)
                s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                s.bind((address, port))
                if not udp:
                    s.listen(8)
                os.write(write, b'R'); os.close(write); write = -1
                while True:
                    if udp:
                        _, peer = s.recvfrom(128)
                        try:
                            s.sendto(b'OK', peer)
                        except OSError:
                            pass
                        continue
                    connection, _ = s.accept()
                    with connection:
                        connection.settimeout(.5)
                        try:
                            if connection.recv(8):
                                connection.sendall(b'OK')
                        except OSError:
                            pass
            except OSError:
                if write >= 0:
                    os.write(write, b'F'); os.close(write)
            finally:
                os._exit(1)
        os.close(write)
        self.assertEqual(os.read(read, 1), b'R')
        os.close(read)
        self.servers.append(pid)
        return pid

    def request(self, uid, address, port, udp=False):
        read, write = os.pipe()
        pid = os.fork()
        if pid == 0:
            os.close(read)
            os.setgroups([]); os.setgid(uid); os.setuid(uid)
            try:
                with socket.socket(socket.AF_INET, socket.SOCK_DGRAM if udp else socket.SOCK_STREAM) as s:
                    s.settimeout(.4); s.connect((address, port))
                    s.settimeout(.4); s.sendall(b'test')
                    value = b'Y' if s.recv(2) == b'OK' else b'N'
            except OSError:
                value = b'N'
            os.write(write, value); os.close(write); os._exit(0)
        os.close(write)
        value = os.read(read, 1)
        os.close(read); os.waitpid(pid, 0)
        return value == b'Y'

    def tearDown(self):
        self.stop_servers()

    def test_01_only_controller_reaches_control_gateway(self):
        self.listener(1002, '127.0.0.1', 19080)
        self.assertTrue(self.request(7501, '127.0.0.1', 19080))
        for uid in [0, 1000, 1001, 1003, 7502]:
            self.assertFalse(self.request(uid, '127.0.0.1', 19080), uid)

    def test_02_only_outer_gateway_reaches_inner_gateway(self):
        self.listener(1003, '127.0.0.1', 18080)
        self.assertTrue(self.request(1002, '127.0.0.1', 18080))
        for uid in [0, 1000, 1001, 7501, 7502]:
            self.assertFalse(self.request(uid, '127.0.0.1', 18080), uid)

    def test_03_other_uids_cannot_impersonate_gateway_or_controller(self):
        for owner, port, client in [(1000, 19080, 7501), (1000, 18080, 1002), (1000, 8000, 1056077)]:
            self.listener(owner, '127.0.0.1', port)
            self.assertFalse(self.request(client, '127.0.0.1', port))
            self.stop_servers()

    def test_04_masking_private_http_is_uid_scoped(self):
        self.listener(1001, '127.0.0.3', 28500)
        self.assertTrue(self.request(1001, '127.0.0.3', 28500))
        for uid in [0, 1000, 1002, 1003, 7501]:
            self.assertFalse(self.request(uid, '127.0.0.3', 28500), uid)
        self.stop_servers()
        self.listener(1000, '127.0.0.3', 28500)
        self.assertFalse(self.request(1001, '127.0.0.3', 28500))

    def test_05_no_raw_worker_tcp_for_native_roles(self):
        self.listener(0, '127.0.0.1', 19741)
        for uid in [1000, 1001, 1002, 1003, 7501]:
            self.assertFalse(self.request(uid, '127.0.0.1', 19741), uid)

    def test_06_native_gateway_and_desktop_egress(self):
        self.listener(1003, '127.0.0.1', 8765)
        self.assertTrue(self.request(1003, '127.0.0.1', 8765))
        self.assertFalse(self.request(1000, '127.0.0.1', 8765))
        self.listener(1056077, '127.0.0.1', 1080)
        self.assertTrue(self.request(1000, '127.0.0.1', 1080))
        self.assertFalse(self.request(1001, '127.0.0.1', 1080))

    def test_07_invalid_runtime_uids_are_refused(self):
        for uid in [0, 1000, 1001, 1002, 1003, 5000, 1048576, True, '7501; flush ruleset']:
            with self.assertRaises(ValueError):
                policy_module.policy(uid)

    def test_08_only_front_can_reach_owner_controller(self):
        self.listener(7501, '127.0.0.1', 8000)
        self.assertTrue(self.request(1056077, '127.0.0.1', 8000))
        for uid in [0, 1000, 1001, 1002, 1003, 7501, 65534]:
            self.assertFalse(self.request(uid, '127.0.0.1', 8000), uid)
        self.listener(0, '127.0.0.1', 19741)
        self.assertFalse(self.request(65534, '127.0.0.1', 19741))

    def test_09_fixed_service_owners_prevent_impersonation(self):
        for owner, port, client, udp in [
            (1003, 6080, 1003, False), (1003, 8765, 1003, False),
            (1000, 5900, 1003, False), (1000, 47990, 1003, False),
            (1000, 47998, 1003, True), (1056077, 1080, 1000, False),
        ]:
            with self.subTest(port=port, udp=udp):
                self.listener(owner, '127.0.0.1', port, udp)
                self.assertTrue(self.request(client, '127.0.0.1', port, udp))
                self.stop_servers()
                self.listener(65534, '127.0.0.1', port, udp)
                self.assertFalse(self.request(client, '127.0.0.1', port, udp))
                self.stop_servers()


if __name__ == '__main__':
    result = unittest.TextTestRunner(verbosity=2).run(unittest.defaultTestLoader.loadTestsFromTestCase(NetworkChecks))
    receipt = {'schema': 'kvm-box-native-network-check/1', 'testsRun': result.testsRun,
        'failures': len(result.failures), 'errors': len(result.errors),
        'scope': 'Real TCP packet and nftables UID checks in an isolated network namespace. No full profile, worker Unix broker, desktop or hardware qualification.'}
    (OUT / 'network-result.json').write_text(json.dumps(receipt, indent=2) + '\n')
    raise SystemExit(0 if result.wasSuccessful() else 1)
