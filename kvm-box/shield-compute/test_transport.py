"""Opt-in tests against a real worker, through a fault-injecting TCP proxy.

Only synthetic fixtures are used. Do not run against a production reservation.
"""
import hashlib
import os
import socket
import struct
import threading
import unittest

import numpy as np

from channel import ChannelError, MaskedChannel
from compiler import compile_graph, constant_digest, execute

LIBRARY = os.environ.get('SHIELD_COMPUTE_TEST_LIBRARY')
PORT = int(os.environ.get('SHIELD_COMPUTE_TEST_PORT', '0'))


def receive(sock, length):
    buf = bytearray()
    while len(buf) < length:
        part = sock.recv(length - len(buf))
        if not part:
            raise EOFError()
        buf.extend(part)
    return bytes(buf)


class Proxy:
    def __init__(self, corrupt=False, corrupt_at=1):
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(1)
        self.listener.settimeout(15)
        self.corrupt = corrupt
        self.corrupt_at = corrupt_at
        self.masked_hashes = []
        self.commands = []
        self.failure = None
        self.thread = threading.Thread(target=self.run, daemon=True)
        self.thread.start()

    def run(self):
        try:
            with self.listener.accept()[0] as guest, socket.create_connection(('127.0.0.1', PORT), 15) as worker:
                guest.settimeout(15)
                worker.settimeout(15)
                while True:
                    header = receive(guest, 9)
                    command, size = struct.unpack('<BQ', header)
                    if size > 16 * 1024 * 1024:
                        raise RuntimeError('oversized test frame')
                    payload = receive(guest, size)
                    self.commands.append(command)
                    if command in (12, 13):
                        # n, m and the one node id occupy the first 12 bytes.
                        self.masked_hashes.append(hashlib.sha256(payload[12:]).digest())
                    worker.sendall(header + payload)
                    reply_header = receive(worker, 9)
                    status, size = struct.unpack('<BQ', reply_header)
                    if size > 16 * 1024 * 1024:
                        raise RuntimeError('oversized test reply')
                    reply = bytearray(receive(worker, size))
                    if (self.corrupt and command in (12, 13) and status == 0 and reply
                            and len(self.masked_hashes) == self.corrupt_at):
                        reply[0] ^= 1
                    guest.sendall(reply_header + reply)
        except EOFError:
            pass
        except BaseException as error:
            self.failure = error
        finally:
            self.listener.close()

    def join(self):
        self.thread.join(20)
        if self.thread.is_alive():
            raise RuntimeError('test proxy did not terminate')
        if self.failure:
            raise self.failure


@unittest.skipUnless(LIBRARY and PORT, 'requires dedicated Shield worker and core library')
class TransportTests(unittest.TestCase):
    def setUp(self):
        rng = np.random.default_rng(17)
        self.w = rng.integers(-2, 3, (512, 256), dtype=np.int64)
        self.x = rng.integers(-100, 101, (2, 512), dtype=np.int64)

    def test_repeated_plaintext_uses_distinct_masks_and_exact_verified_outputs(self):
        proxy = Proxy()
        with MaskedChannel(LIBRARY, '127.0.0.1', proxy.port, [constant_digest(self.w)]) as channel:
            node = channel.register(self.w, 2)
            channel.start()
            for _ in range(2):
                np.testing.assert_array_equal(channel.matmul(node, self.x), self.x @ self.w)
            self.assertEqual(channel.stats['exchanges'], 2)
        proxy.join()
        self.assertEqual(len(proxy.masked_hashes), 2)
        self.assertNotEqual(*proxy.masked_hashes)
        self.assertTrue(set(proxy.commands).issubset({0, 1, 2, 8, 9, 10, 11, 12, 13, 15}))

    def test_modified_real_gpu_reply_retires_channel(self):
        proxy = Proxy(corrupt=True)
        with MaskedChannel(LIBRARY, '127.0.0.1', proxy.port, [constant_digest(self.w)]) as channel:
            node = channel.register(self.w, 2)
            channel.start()
            with self.assertRaisesRegex(ChannelError, 'no result released'):
                channel.matmul(node, self.x)
            self.assertEqual(channel.stats['verification_failures'], 1)
            with self.assertRaisesRegex(ChannelError, 'retired'):
                channel.matmul(node, self.x)
        proxy.join()
        self.assertEqual(len(proxy.masked_hashes), 1)

    def test_transport_independently_refuses_unapproved_weight(self):
        # Port zero is invalid, so use an unused loopback port. No connection
        # occurs: registration fails before sh_link_start can upload anything.
        with socket.socket() as sock:
            sock.bind(('127.0.0.1', 0))
            port = sock.getsockname()[1]
            with MaskedChannel(LIBRARY, '127.0.0.1', port) as channel:
                with self.assertRaisesRegex(ChannelError, 'not approved'):
                    channel.register(self.w, 2)
                self.assertEqual(channel.stats['exchanges'], 0)

    def tiled_graph(self, rows):
        return {'schema': 'shield-graph/1', 'nodes': [
            {'name': 'x', 'op': 'input', 'shape': [rows, 32], 'bound': 10},
            {'name': 'w', 'op': 'constant', 'shape': list(self.w.shape), 'value': self.w.tolist()},
            {'name': 'y', 'op': 'matmul', 'args': ['x', 'w']},
            {'name': 'relu', 'op': 'relu', 'args': ['y']},
        ], 'outputs': ['relu']}

    def test_tiled_graph_uses_one_link_and_fresh_masks(self):
        self.w = np.random.default_rng(18).integers(-2, 3, (32,8), dtype=np.int64)
        for rows in (65, 129, 4096):
            with self.subTest(rows=rows):
                proxy = Proxy()
                approved = [constant_digest(self.w)]
                channel = MaskedChannel(LIBRARY, '127.0.0.1', proxy.port, approved)
                x = np.full((rows, 32), 3, dtype=np.int64)
                outputs, routes = execute(compile_graph(self.tiled_graph(rows), approved), {'x': x}, channel)
                np.testing.assert_array_equal(outputs['relu'], np.maximum(x @ self.w, 0))
                self.assertEqual(routes['y'], 'masked-field-gemm')
                self.assertEqual(channel.stats['exchanges'], (rows+63)//64)
                self.assertEqual(channel.stats['macs'], rows*32*self.w.shape[1])
                self.assertEqual(channel.stats['verification_failures'], 0)
                proxy.join()
                self.assertEqual(len(set(proxy.masked_hashes)), (rows+63)//64)

    def test_later_tiled_reply_corruption_releases_no_graph_output(self):
        self.w = np.random.default_rng(18).integers(-2, 3, (32,8), dtype=np.int64)
        for corrupt_at in (2, 3):
            with self.subTest(corrupt_at=corrupt_at):
                proxy = Proxy(corrupt=True, corrupt_at=corrupt_at)
                approved = [constant_digest(self.w)]
                channel = MaskedChannel(LIBRARY, '127.0.0.1', proxy.port, approved)
                with self.assertRaisesRegex(ChannelError, 'no result released'):
                    execute(compile_graph(self.tiled_graph(129), approved),
                            {'x': np.full((129,32), 3, dtype=np.int64)}, channel)
                self.assertEqual(channel.stats['verification_failures'], 1)
                with self.assertRaisesRegex(ChannelError, 'retired'):
                    channel.start()
                proxy.join()
                self.assertEqual(len(proxy.masked_hashes), corrupt_at)


if __name__ == '__main__':
    unittest.main()
