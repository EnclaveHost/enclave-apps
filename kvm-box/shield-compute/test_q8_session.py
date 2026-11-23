#!/usr/bin/env python3
"""Executor sessions end to end: the service hands a client socket to the real
executor, and the client speaks the binary protocol directly.

Run inside the desktop OCI image with this tree at /src, the public models at
/models and the dedicated test worker on loopback 9741 (as for
opencl/probe_q8_masked.py). Public tensors and synthetic activations only; not
confidential-VM evidence.
"""
import base64
import fcntl
import json
import mmap
import os
from pathlib import Path
import select
import socket
import struct
import sys
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))
import numpy as np
from service import Compute, Server, Handler, receive, send

ARITHMETIC = 'shield-fixed-point-v1'
OPEN, MATMUL, SHARE, MATMUL_SHARED, MAIL, WAKE, ARENA, BYE = 1, 2, 3, 4, 5, 6, 7, 0xff
REGION = 64 << 20
NO_PAIR = 0xffffffff
F_GET_SEALS, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_GROW = 1034, 1, 2, 4
SOCKET = '/tmp/shield-q8-session-test.sock'


def exact(sock, size):
    data = bytearray()
    while len(data) < size:
        part = sock.recv(size - len(data))
        if not part:
            raise EOFError(bytes(data))
        data.extend(part)
    return bytes(data)


class Session:
    def __init__(self, engine='llama'):
        self.sock = socket.socket(socket.AF_UNIX)
        self.sock.settimeout(60)
        self.sock.connect(SOCKET)
        send(self.sock, {'op': 'q8_session', 'engine': engine, 'arithmetic': ARITHMETIC})
        self.reply = receive(self.sock, 4096)
        self.shared = None

    def close(self):
        if self.shared is not None:
            self.shared.close()
        self.sock.close()

    def open(self, slot, tensor):
        name = tensor['name'].encode()
        self.sock.sendall(struct.pack('<4I2Q64s', OPEN, slot, len(name), 0, tensor['ne'][0], tensor['ne'][1],
                                      tensor['sha256'].encode()) + name)
        kind, ident, status, _ = struct.unpack('<4I', exact(self.sock, 16))
        assert (kind, ident) == (OPEN, slot)
        return status

    def matmul(self, slot, x, n):
        m = x.shape[0]
        self.sock.sendall(struct.pack('<4I', MATMUL, slot, m, 0) + x.astype('<f4').tobytes())
        kind, ident, status, _, exchanges, macs = struct.unpack('<4I2Q', exact(self.sock, 32))
        assert (kind, ident) == (MATMUL, slot)
        if status:
            return status, None
        return 0, np.frombuffer(exact(self.sock, m * n * 4), dtype='<f4').reshape(m, n)

    def share(self):
        self.sock.sendall(struct.pack('<4I', SHARE, 0, 0, 0))
        data, fds, _, _ = socket.recv_fds(self.sock, 16, 1)
        kind, _, status, region = struct.unpack('<4I', data)
        assert kind == SHARE
        if status:
            return None
        assert region == REGION and len(fds) == 1
        self.fd = fds[0]
        self.shared = mmap.mmap(self.fd, 4 * REGION)
        return self.fd

    def matmul_shared(self, slot, x, n, pair=NO_PAIR, pair_n=0):
        m = x.shape[0]
        raw = x.astype('<f4').tobytes()
        self.shared[:len(raw)] = raw
        self.sock.sendall(struct.pack('<6I', MATMUL_SHARED, slot, m, 0, pair, 0))
        kind, ident, status, paired, exchanges, macs = struct.unpack('<4I2Q', exact(self.sock, 32))
        assert (kind, ident) == (MATMUL_SHARED, slot)
        if status:
            return status, None, None
        first = np.frombuffer(self.shared[REGION:REGION + m*n*4], dtype='<f4').reshape(m, n).copy()
        second = (np.frombuffer(self.shared[2*REGION:2*REGION + m*pair_n*4], dtype='<f4').reshape(m, pair_n).copy()
                  if paired else None)
        return 0, first, second


    # The mailbox is the executor's Mail in the last bytes of the shared memory:
    # request, reply, sleeping, id, m, pair, status, paired (uint32 each),
    # exchanges, macs (uint64), third, reserved, in_arena, out_arena (uint32),
    # in_offset, out_offset (uint64).
    MAILBOX = 4 * REGION - 80

    def mail(self):
        self.sock.sendall(struct.pack('<4I', MAIL, 0, 0, 0))
        kind, _, status, _ = struct.unpack('<4I', exact(self.sock, 16))
        assert kind == MAIL
        return status

    def arena(self, index, size):
        self.sock.sendall(struct.pack('<4I', ARENA, index, size & 0xffffffff, size >> 32))
        data, fds, _, _ = socket.recv_fds(self.sock, 16, 1)
        kind, ident, status, _ = struct.unpack('<4I', data)
        assert (kind, ident) == (ARENA, index)
        if status:
            assert not fds
            return None, None
        return fds[0], mmap.mmap(fds[0], size)

    def mailed(self, slot, m, in_place=(0, 0), out_place=(0, 0)):
        at = self.MAILBOX
        request = struct.unpack_from('<I', self.shared, at)[0] + 1
        struct.pack_into('<3I', self.shared, at + 12, slot, m, NO_PAIR)
        struct.pack_into('<4I2Q', self.shared, at + 48, 0, 0, in_place[0], out_place[0], in_place[1], out_place[1])
        struct.pack_into('<I', self.shared, at, request)
        try:
            self.sock.sendall(struct.pack('<4I', WAKE, 0, 0, 0))  # in case it sleeps; ignored otherwise
        except OSError:
            return None
        deadline = time.monotonic() + 60
        while struct.unpack_from('<I', self.shared, at + 4)[0] != request:
            if time.monotonic() > deadline:
                return None
            if select.select([self.sock], [], [], 0)[0]:   # a dropped session is closed, not answered
                try:
                    if not self.sock.recv(16, socket.MSG_PEEK):
                        return None
                except OSError:
                    return None
            time.sleep(0.0005)
        return struct.unpack_from('<I', self.shared, at + 24)[0]


class Sessions(unittest.TestCase):
    # These tests count exchanges and compare bytes between requests.
    exchange_every_product = True

    @classmethod
    def setUpClass(cls):
        config = json.loads(Path('/shield/service.json').read_text())
        config.update(jobs='/tmp', enableQ8Experimental=True, timeoutSeconds=120, clientUids=[os.getuid()],
                      socket=SOCKET, q8ExchangeEveryProduct=cls.exchange_every_product)
        config['worker'] = {'host': '127.0.0.1', 'port': 9741}
        cls.config = config
        cls.compute = Compute(config)
        # The mailbox is an experiment the executor allows only when told to.
        environment = cls.compute.environment
        def with_mailbox(*args):
            env = environment(*args)
            env['SHIELD_Q8_MAILBOX'] = '1'
            return env
        cls.compute.environment = with_mailbox
        if os.path.exists(SOCKET):
            os.unlink(SOCKET)
        cls.server = Server(SOCKET, Handler)
        cls.server.compute = cls.compute
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        tensors = {t['name']: t for t in json.loads(Path(config['models']['llama']['policy']).read_text())['tensors']}
        cls.gate, cls.up, cls.down = (tensors['blk.3.ffn_%s.weight' % part] for part in ('gate', 'up', 'down'))
        cls.rng = np.random.default_rng(20261119)

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join()
        if cls.compute.q8_broker:
            cls.compute.q8_broker.close()

    def activation(self, tensor, rows=1):
        return (self.rng.standard_normal((rows, tensor['ne'][0])) * 0.5).astype('<f4')

    def legacy(self, tensor, x):
        with socket.socket(socket.AF_UNIX) as sock:
            sock.settimeout(60)
            sock.connect(SOCKET)
            send(sock, {'op': 'q8_0_matmul', 'engine': 'llama', 'tensor': tensor['name'], 'sha256': tensor['sha256'],
                        'k': tensor['ne'][0], 'n': tensor['ne'][1], 'm': x.shape[0], 'arithmetic': ARITHMETIC,
                        'input': base64.b64encode(x.tobytes()).decode()})
            reply = receive(sock, 16 << 20)
        self.assertTrue(reply.get('ok') and reply.get('supported'), reply)
        return np.frombuffer(base64.b64decode(reply['output']), dtype='<f4').reshape(x.shape[0], tensor['ne'][1])

    def assert_healthy(self):
        self.assertEqual(self.compute.status()['state'], 'ready')

    def test_products_match_the_request_protocol(self):
        session = Session()
        self.addCleanup(session.close)
        self.assertEqual(session.reply, {'ok': True, 'session': 'shield-q8-session/1'})
        self.assertEqual(session.open(0, self.down), 0)
        for rows in (1, 3, 32):
            x = self.activation(self.down, rows)
            status, y = session.matmul(0, x, self.down['ne'][1])
            self.assertEqual(status, 0)
            self.assertTrue(np.isfinite(y).all())
            self.assertEqual(y.tobytes(), self.legacy(self.down, x).tobytes())
        self.assert_healthy()

    def test_shared_memory_is_sealed_and_gives_the_same_products(self):
        session = Session()
        self.addCleanup(session.close)
        fd = session.share()
        self.assertIsNotNone(fd)
        seals = fcntl.fcntl(fd, F_GET_SEALS)
        self.assertEqual(seals & (F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW), F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW)
        self.assertEqual(os.fstat(fd).st_size, 4 * REGION)
        for size in (0, REGION, 5 * REGION):
            with self.assertRaises(OSError):
                os.ftruncate(fd, size)
        self.assertEqual(session.open(0, self.gate), 0)
        self.assertEqual(session.open(1, self.up), 0)
        x = self.activation(self.gate)
        status, gate, up = session.matmul_shared(0, x, self.gate['ne'][1], pair=1, pair_n=self.up['ne'][1])
        self.assertEqual(status, 0)
        self.assertIsNotNone(up)
        status, alone, nothing = session.matmul_shared(1, x, self.up['ne'][1])
        self.assertEqual(status, 0)
        self.assertIsNone(nothing)
        self.assertEqual(up.tobytes(), alone.tobytes())
        status, socket_gate = session.matmul(0, x, self.gate['ne'][1])
        self.assertEqual(gate.tobytes(), socket_gate.tobytes())
        self.assertEqual(gate.tobytes(), self.legacy(self.gate, x).tobytes())
        self.assertEqual(up.tobytes(), self.legacy(self.up, x).tobytes())
        # A pair with a different input width, itself or an unopened slot is declined, not an error.
        self.assertEqual(session.open(2, self.down), 0)
        for pair in (2, 0, 9):
            status, first, second = session.matmul_shared(0, x, self.gate['ne'][1], pair=pair, pair_n=1)
            self.assertEqual(status, 0)
            self.assertIsNone(second)
            self.assertEqual(first.tobytes(), gate.tobytes())
        self.assert_healthy()

    def test_a_continuation_computed_ahead_answers_only_the_bytes_it_was_computed_from(self):
        # After a few rounds the executor expects the gated pair to be followed
        # by the down projection and computes it before it is asked. A zero
        # activation makes the gated product exactly zero on any CPU, so the
        # expectation is certain to be learned here.
        session = Session()
        self.addCleanup(session.close)
        self.assertIsNotNone(session.share())
        for slot, tensor in enumerate((self.gate, self.up, self.down)):
            self.assertEqual(session.open(slot, tensor), 0)
        width, hidden_width, out = self.gate['ne'][0], self.down['ne'][0], self.down['ne'][1]
        zeros = np.zeros((1, width), dtype='<f4')
        hidden = np.zeros((1, hidden_width), dtype='<f4')
        want = self.legacy(self.down, hidden)

        def pair():
            status, gate, up = session.matmul_shared(0, zeros, hidden_width, pair=1, pair_n=hidden_width)
            self.assertEqual(status, 0)
            self.assertFalse(gate.any() or up.any())

        for _ in range(4):
            pair()
            status, y, _ = session.matmul_shared(2, hidden, out)
            self.assertEqual(status, 0)
            np.testing.assert_array_equal(y, want)
        # A different activation after the pair gets its own product.
        pair()
        other = self.activation(self.down)
        status, y, _ = session.matmul_shared(2, other, out)
        self.assertEqual(status, 0)
        np.testing.assert_array_equal(y, self.legacy(self.down, other))
        # So does a different tensor in the expected slot, and the same bytes
        # for a tensor that is not the expected one.
        pair()
        status, y, _ = session.matmul_shared(0, zeros, hidden_width)
        self.assertEqual(status, 0)
        self.assertFalse(y.any())
        for _ in range(2):
            pair()
            status, y, _ = session.matmul_shared(2, hidden, out)
            self.assertEqual(status, 0)
            np.testing.assert_array_equal(y, want)
        self.assert_healthy()

    def test_unapproved_references_are_refused_without_harm(self):
        session = Session()
        self.addCleanup(session.close)
        wrong_digest = dict(self.gate, sha256='0' * 64)
        wrong_shape = dict(self.gate, ne=[self.gate['ne'][0], self.gate['ne'][1] + 8, 1, 1])
        unknown = dict(self.gate, name='blk.3.not_a_tensor.weight')
        for slot, tensor in enumerate((wrong_digest, wrong_shape, unknown)):
            self.assertEqual(session.open(slot, tensor), 2)
        self.assertEqual(session.open(3, self.gate), 0)
        status, y = session.matmul(3, self.activation(self.gate), self.gate['ne'][1])
        self.assertEqual(status, 0)
        self.assert_healthy()

    def test_client_mistakes_end_only_that_session(self):
        bystander = Session()
        self.addCleanup(bystander.close)
        self.assertEqual(bystander.open(0, self.down), 0)
        x = self.activation(self.down)
        bad = x.copy()
        bad[0, 5] = np.inf
        nan = x.copy()
        nan[0, 7] = np.nan
        mistakes = {
            'non-finite': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 0, 1, 0) + bad.tobytes()),
            'nan': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 0, 1, 0) + nan.tobytes()),
            'unknown frame': lambda s: s.sock.sendall(struct.pack('<4I', 77, 0, 0, 0)),
            'unopened slot': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 5, 1, 0)),
            'too many rows': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 0, 33, 0)),
            'zero rows': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 0, 0, 0)),
            'reserved field': lambda s: s.sock.sendall(struct.pack('<4I', MATMUL, 0, 1, 1) + x.tobytes()),
            'shared without memory': lambda s: s.sock.sendall(struct.pack('<6I', MATMUL_SHARED, 0, 1, 0, NO_PAIR, 0)),
            'oversized name': lambda s: s.sock.sendall(struct.pack('<4I', OPEN, 1, 4096, 0)),
            'slot out of range': lambda s: s.sock.sendall(struct.pack('<4I', OPEN, 1 << 20, 8, 0)),
        }
        for label, mistake in mistakes.items():
            with self.subTest(label):
                session = Session()
                self.assertEqual(session.open(0, self.down), 0)
                mistake(session)
                with self.assertRaises((EOFError, ConnectionError)):
                    exact(session.sock, 16)
                session.close()
                status, y = bystander.matmul(0, x, self.down['ne'][1])
                self.assertEqual(status, 0)
                self.assert_healthy()

    def test_stalled_client_loses_its_session_and_others_continue(self):
        stalled, working = Session(), Session()
        self.addCleanup(stalled.close)
        self.addCleanup(working.close)
        self.assertEqual(stalled.open(0, self.down), 0)
        self.assertEqual(working.open(0, self.down), 0)
        x = self.activation(self.down)
        stalled.sock.sendall(struct.pack('<4I', MATMUL, 0, 1, 0) + x.tobytes()[:100])
        start = time.monotonic()
        status, y = working.matmul(0, x, self.down['ne'][1])   # waits behind the stalled frame
        self.assertEqual(status, 0)
        self.assertLess(time.monotonic() - start, 20)
        with self.assertRaises((EOFError, ConnectionError)):
            exact(stalled.sock, 16)
        self.assert_healthy()

    def test_sessions_are_bounded_and_interleave(self):
        sessions = [Session() for _ in range(16)]
        for session in sessions:
            self.addCleanup(session.close)
            self.assertTrue(session.reply.get('ok'), session.reply)
        extra = Session()
        self.addCleanup(extra.close)
        self.assertEqual(extra.reply, {'ok': False, 'error': 'compute busy'})
        x = self.activation(self.down)
        expected = None
        for session in sessions[:4]:
            self.assertEqual(session.open(0, self.down), 0)
        for _ in range(3):
            for session in sessions[:4]:
                status, y = session.matmul(0, x, self.down['ne'][1])
                self.assertEqual(status, 0)
                expected = expected or y.tobytes()
                self.assertEqual(y.tobytes(), expected)
        self.assert_healthy()

    def test_invalid_session_requests(self):
        for request in ({'op': 'q8_session', 'engine': 'llama'},
                        {'op': 'q8_session', 'engine': 'llama', 'arithmetic': 'native-model-v1'},
                        {'op': 'q8_session', 'engine': 'nothing', 'arithmetic': ARITHMETIC},
                        {'op': 'q8_session', 'engine': 'llama', 'arithmetic': ARITHMETIC, 'path': '/x'}):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(30)
                sock.connect(SOCKET)
                send(sock, request)
                reply = receive(sock, 4096)
                self.assertFalse(reply.get('ok'), request)
        self.assert_healthy()

    def test_warm_hint_starts_the_executor_without_computing(self):
        def hint(request):
            with socket.socket(socket.AF_UNIX) as sock:
                sock.settimeout(60)
                sock.connect(SOCKET)
                send(sock, request)
                return receive(sock, 4096)
        with self.compute.q8_lock:
            if self.compute.q8_broker:
                self.compute.q8_broker.close()
        before = self.compute.status()
        self.assertFalse(hint({'op': 'q8_warm'}).get('ok'))
        self.assertFalse(hint({'op': 'q8_warm', 'arithmetic': 'native-model-v1'}).get('ok'))
        self.assertFalse(hint({'op': 'q8_warm', 'arithmetic': ARITHMETIC, 'engine': 'sd'}).get('ok'))
        self.assertEqual(hint({'op': 'q8_warm', 'arithmetic': ARITHMETIC}), {'ok': True, 'warm': True})
        self.assertEqual(hint({'op': 'q8_warm', 'arithmetic': ARITHMETIC}), {'ok': True, 'warm': True})
        self.assertIsNotNone(self.compute.q8_broker.process)
        after = self.compute.status()
        self.assertEqual(after['telemetry'], before['telemetry'])
        self.assertEqual(after['completed'], before['completed'])
        session = Session()
        self.addCleanup(session.close)
        self.assertEqual(session.open(0, self.down), 0)
        x = self.activation(self.down)
        status, y = session.matmul(0, x, self.down['ne'][1])
        self.assertEqual(status, 0)
        self.assertEqual(y.tobytes(), self.legacy(self.down, x).tobytes())
        # The other engine's session replaces the warmed executor.
        other = Session('sd')
        self.addCleanup(other.close)
        self.assertTrue(other.reply.get('ok'), other.reply)
        self.assert_healthy()

    def test_verified_work_is_counted_and_a_planned_stop_says_goodbye(self):
        session = Session()
        self.addCleanup(session.close)
        self.assertEqual(session.open(0, self.down), 0)
        before = self.compute.status()
        x = self.activation(self.down)
        for _ in range(5):
            self.assertEqual(session.matmul(0, x + self.rng.standard_normal(x.shape).astype('<f4') * 0.01,
                                            self.down['ne'][1])[0], 0)
        after = self.compute.status()
        self.assertEqual(after['completed'] - before['completed'], 5)
        self.assertEqual(after['telemetry']['verifiedOffloads'] - before['telemetry']['verifiedOffloads'], 5)
        self.assertEqual(after['telemetry']['maskedMacs'] - before['telemetry']['maskedMacs'],
                         5 * self.down['ne'][0] * self.down['ne'][1])
        self.assertEqual(self.compute.status(), after)   # reading it twice adds nothing
        # What a native or graph job does first: stop the executor between requests.
        with self.compute.q8_lock:
            self.compute.q8_broker.close()
        kind, _, _, _ = struct.unpack('<4I', exact(session.sock, 16))
        self.assertEqual(kind, BYE)
        with self.assertRaises(EOFError):
            exact(session.sock, 1)
        final = self.compute.status()
        self.assertEqual(final['state'], 'ready')
        self.assertEqual(final['telemetry']['verifiedOffloads'], after['telemetry']['verifiedOffloads'])
        # A new session starts a fresh executor.
        again = Session()
        self.addCleanup(again.close)
        self.assertEqual(again.open(0, self.down), 0)
        self.assertEqual(again.matmul(0, x, self.down['ne'][1])[0], 0)
        self.assert_healthy()

    def mailbox_session(self):
        session = Session()
        self.addCleanup(session.close)
        self.assertIsNotNone(session.share())
        self.assertEqual(session.open(0, self.down), 0)
        self.assertEqual(session.mail(), 0)
        return session

    def test_an_arena_is_sealed_and_a_mailed_request_reads_and_writes_in_it(self):
        session = self.mailbox_session()
        k, n = self.down['ne'][:2]
        size = 1 << 20
        for index, bad in ((1, size), (0, 0), (0, size + 1), (0, (1 << 30) + 4096)):
            self.assertEqual(session.arena(index, bad), (None, None))
        fd, arena = session.arena(0, size)
        self.assertIsNotNone(fd)
        self.addCleanup(arena.close)
        seals = fcntl.fcntl(fd, F_GET_SEALS)
        self.assertEqual(seals & (F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW), F_SEAL_SEAL | F_SEAL_SHRINK | F_SEAL_GROW)
        self.assertEqual(os.fstat(fd).st_size, size)
        for rows in (1, 2):
            x = self.activation(self.down, rows)
            raw = x.tobytes()
            # through the regions
            session.shared[:len(raw)] = raw
            self.assertEqual(session.mailed(0, rows), 0)
            reference = bytes(session.shared[REGION:REGION + rows * n * 4])
            session.shared[REGION:REGION + rows * n * 4] = bytes(rows * n * 4)
            # the same activation in the arena, the product beside it
            where, to = 4096 + 12, 8 * 4096 + 4 + rows * k * 4
            arena[where:where + len(raw)] = raw
            session.shared[:len(raw)] = bytes(len(raw))
            self.assertEqual(session.mailed(0, rows, (1, where), (1, to)), 0)
            self.assertEqual(bytes(arena[to:to + rows * n * 4]), reference)
            self.assertEqual(bytes(session.shared[REGION:REGION + rows * n * 4]), bytes(rows * n * 4))
            # only the product in the arena
            session.shared[:len(raw)] = raw
            arena[to:to + rows * n * 4] = bytes(rows * n * 4)
            self.assertEqual(session.mailed(0, rows, (0, 0), (1, to)), 0)
            self.assertEqual(bytes(arena[to:to + rows * n * 4]), reference)
        self.assert_healthy()

    def test_a_place_outside_an_arena_ends_the_session_only(self):
        k, n = self.down['ne'][:2]
        size = 1 << 20
        for in_place, out_place in (((2, 0), (0, 0)), ((1, size - k * 4 + 4), (0, 0)), ((0, 0), (1, size - n * 4 + 4)),
                                    ((0, 0), (1, 1 << 40))):
            session = self.mailbox_session()
            fd, arena = session.arena(0, size)
            self.assertIsNotNone(fd)
            self.addCleanup(arena.close)
            raw = self.activation(self.down).tobytes()
            session.shared[:len(raw)] = raw
            self.assertIsNone(session.mailed(0, 1, in_place, out_place))
            with self.assertRaises((EOFError, OSError)):
                session.sock.settimeout(10)
                while True:
                    exact(session.sock, 16)
        other = self.mailbox_session()
        raw = self.activation(self.down).tobytes()
        other.shared[:len(raw)] = raw
        self.assertEqual(other.mailed(0, 1), 0)
        self.assert_healthy()


class PadsRunOut(unittest.TestCase):
    """The default: a product whose pad pool is empty may be computed by the executor itself."""
    exchange_every_product = False
    setUpClass = classmethod(Sessions.setUpClass.__func__)
    tearDownClass = classmethod(Sessions.tearDownClass.__func__)
    activation, legacy, assert_healthy = Sessions.activation, Sessions.legacy, Sessions.assert_healthy

    def test_products_stay_correct_and_counted_when_the_pool_is_empty(self):
        session = Session()
        self.addCleanup(session.close)
        self.assertEqual(session.open(0, self.down), 0)
        before = self.compute.status()
        requests = 48   # faster than one refill thread fills a pool that starts empty
        for _ in range(requests):
            x = self.activation(self.down)
            status, y = session.matmul(0, x, self.down['ne'][1])
            self.assertEqual(status, 0)
            self.assertTrue(np.isfinite(y).all())
            # The line protocol always exchanges. The exchange and the CPU
            # backend each quantise the activation their own way, so on these
            # random activations the products agree to several percent and not
            # bit for bit (6% seen).
            reference = self.legacy(self.down, x)
            self.assertLess(np.linalg.norm(y - reference), 0.12 * np.linalg.norm(reference))
        after = self.compute.status()
        self.assertEqual(after['completed'] - before['completed'], 2 * requests)
        verified = after['telemetry']['verifiedOffloads'] - before['telemetry']['verifiedOffloads']
        # Every line-protocol product is an exchange; a session product is one or none.
        self.assertGreaterEqual(verified, requests)
        self.assertLessEqual(verified, 2 * requests)
        self.assert_healthy()


if __name__ == '__main__':
    unittest.main()
