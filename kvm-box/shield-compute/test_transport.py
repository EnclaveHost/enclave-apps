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
from compiler import constant_digest

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
    def __init__(self, corrupt=False):
        self.listener = socket.socket()
        self.listener.bind(('127.0.0.1', 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(1)
        self.listener.settimeout(15)
        self.corrupt = corrupt
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
                    if self.corrupt and command in (12, 13) and status == 0 and reply:
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


if __name__ == '__main__':
    unittest.main()
