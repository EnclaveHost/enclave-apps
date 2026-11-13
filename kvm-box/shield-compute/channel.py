"""Narrow binding to Enclave Shield's existing trusted C implementation.

No CUDA calls and no new cryptography. Use a fresh dedicated process with a
clean SHIELDED_* environment. The host test mode is not a confidential VM.
"""
import ctypes as C
import os
import threading

import numpy as np
from compiler import constant_digest


class ChannelError(RuntimeError):
    def __init__(self, message, *, stage='unknown', code=None):
        super().__init__(message)
        # Only fixed local labels and bounded C return codes may cross the
        # service status boundary. Never forward exception/worker text there.
        self.stage = stage if stage in ('initialize', 'register', 'start', 'compute') else 'unknown'
        self.code = code if type(code) is int and -128 <= code < 0 else None

    def public_failure(self):
        return {'stage': self.stage, 'code': self.code}


class MaskedChannel:
    def __init__(self, library, host, port, approved_public_constants=(),
                 reserve_bytes=128 * 1024 * 1024):
        if any(k.startswith('SHIELDED_') for k in os.environ):
            raise ChannelError('ambient SHIELDED_* configuration is not allowed')
        if not isinstance(host, str) or not host or '\0' in host or not 1 <= port <= 65535:
            raise ValueError('invalid worker endpoint')
        if not 1 <= reserve_bytes <= 1024 * 1024 * 1024:
            raise ValueError('invalid reservation')
        self._lib = C.CDLL(os.fspath(library))
        self._lock = threading.RLock()
        self._weights = []
        self._approved = frozenset(approved_public_constants)
        self._nodes = {}
        self._link = None
        self._started = False
        self._retired = False
        self._stats = (0, 0, 0)
        p64, p8 = C.POINTER(C.c_int64), C.POINTER(C.c_int8)
        signatures = {
            'sh_link_open': ([C.c_char_p, C.c_int, C.c_bool, C.POINTER(C.c_int)], C.c_void_p),
            'sh_link_configure': ([C.c_void_p, C.c_int, C.c_uint64, C.c_int], None),
            'sh_link_configure_shm': ([C.c_void_p, C.c_char_p, C.c_uint64], None),
            'sh_link_add_weight': ([C.c_void_p, C.c_char_p, p8, C.c_int64, C.c_int64, C.c_int32, C.c_int], C.c_int),
            'sh_link_start': ([C.c_void_p], C.c_int),
            'sh_link_gemm': ([C.c_void_p, C.POINTER(C.c_int), C.c_size_t, p64, C.c_int32, C.POINTER(p64)], C.c_int),
            'sh_link_close': ([C.c_void_p], None),
            'sh_link_stats': ([C.c_void_p] + [C.POINTER(C.c_uint64)] * 3, None),
        }
        for name, (args, result) in signatures.items():
            fn = getattr(self._lib, name)
            fn.argtypes, fn.restype = args, result
        err = C.c_int()
        self._link = self._lib.sh_link_open(host.encode(), port, True, C.byref(err))
        if not self._link:
            raise ChannelError(f'Shield initialization failed ({err.value})', stage='initialize', code=err.value)
        self._lib.sh_link_configure(self._link, 0, reserve_bytes, 1)
        self._lib.sh_link_configure_shm(self._link, b'', 0)

    def _check(self):
        if self._retired or not self._link:
            raise ChannelError('channel is retired')

    def _error(self, stage, code):
        self.close()
        # Do not relay arbitrary worker strings or tensor data to host logs.
        raise ChannelError(f'{stage} failed ({code}); channel retired, no result released', stage=stage, code=code)

    def register(self, approved_public_weight, max_rows):
        with self._lock:
            self._check()
            if self._started:
                raise ChannelError('cannot register weights after start')
            w = np.array(approved_public_weight, copy=True)
            if (w.dtype.kind not in 'iu' or w.ndim != 2 or w.shape[0] % 32 or
                    min(w.shape) < 1 or w.size > 16 * 1024 * 1024 or
                    not 1 <= max_rows <= 64 or int(w.min()) < -119 or int(w.max()) > 119):
                raise ValueError('weight is outside FIELD_GEMM contract')
            if constant_digest(w) not in self._approved:
                raise ChannelError('weight is not approved for public disclosure')
            # The C library borrows these bytes for pad refill for its lifetime.
            owned = np.array(w.T, dtype=np.int8, order='C', copy=True)
            node = self._lib.sh_link_add_weight(self._link, f'w{len(self._weights)}'.encode(),
                owned.ctypes.data_as(C.POINTER(C.c_int8)), w.shape[0], w.shape[1], max_rows, -1)
            if node < 0:
                self._error('register', node)
            self._weights.append(owned)
            self._nodes[node] = (w.shape[0], w.shape[1], max_rows,
                                 max(abs(int(w.min())), abs(int(w.max()))))
            return node

    def start(self):
        with self._lock:
            self._check()
            if self._started:
                raise ChannelError('channel already started')
            rc = self._lib.sh_link_start(self._link)
            if rc:
                self._error('start', rc)
            self._started = True

    def matmul(self, node, activation):
        with self._lock:
            self._check()
            if not self._started or node not in self._nodes:
                raise ChannelError('unregistered operation')
            k, n, max_rows, wb = self._nodes[node]
            x = np.array(activation, copy=True)
            if x.dtype.kind not in 'iu' or x.ndim != 2 or x.shape[1] != k or not 1 <= x.shape[0] <= max_rows:
                raise ValueError('invalid activation')
            xb = max(abs(int(x.min())), abs(int(x.max())))
            if xb > (251 * 241 * 239) // 2 or xb * wb * k > (251 * 241 * 239) // 2:
                raise ValueError('activation exceeds the no-wrap field bound')
            x = np.array(x, dtype=np.int64, order='C', copy=True)
            # Partial or corrupt outputs remain private until ALL checks pass.
            scratch = np.empty((x.shape[0], n), dtype=np.int64)
            p64 = C.POINTER(C.c_int64)
            nodes = (C.c_int * 1)(node)
            outputs = (p64 * 1)(scratch.ctypes.data_as(p64))
            rc = self._lib.sh_link_gemm(self._link, nodes, 1, x.ctypes.data_as(p64), x.shape[0], outputs)
            if rc:
                scratch.fill(0)
                self._error('compute', rc)
            return scratch

    @property
    def stats(self):
        with self._lock:
            if self._link:
                counters = [C.c_uint64() for _ in range(3)]
                self._lib.sh_link_stats(self._link, *(C.byref(c) for c in counters))
                self._stats = tuple(c.value for c in counters)
            return dict(zip(('exchanges', 'macs', 'verification_failures'), self._stats))

    def close(self):
        with self._lock:
            self._retired = True
            if self._link:
                self.stats
                self._lib.sh_link_close(self._link)
                self._link = None
            self._weights.clear()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()
