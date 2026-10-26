"""Experimental standard-API Q8 broker; only configured public model weights.

Private activations and results travel over guest-local pipes. The existing
native Shield backend is the sole physical GPU route. This module does not
accept libraries, worker addresses, policies or executable paths from clients.
"""
import base64
import binascii
import hashlib
import json
import os
from pathlib import Path
import select
import signal
import subprocess
import tempfile
import time

import numpy as np

ARITHMETIC = 'shield-fixed-point-v1'
MAX_BYTES = 2 << 20
MAX_EXECUTOR_REQUESTS = 100000
MAX_WEIGHT_BYTES = 512 << 20
STATS = ('offloaded', 'local', 'graph_macs', 'exchanges', 'macs', 'verification_failures')


class Q8ExecutionError(RuntimeError):
    pass


def finite_f32(data):
    # Classify every little-endian IEEE-754 value without a Python loop. The
    # array is a read-only view of these immutable local message bytes; this
    # neither caches validation nor changes NaN/infinity rejection. NumPy is
    # already part of the measured guest's CPU graph runtime.
    return len(data) % 4 == 0 and bool(np.isfinite(np.frombuffer(data, dtype='<f4')).all())


def validate_request(request, config):
    if (not isinstance(request, dict) or set(request) !=
            {'op', 'engine', 'tensor', 'sha256', 'k', 'n', 'm', 'arithmetic', 'input'} or
            request['op'] != 'q8_0_matmul' or request['arithmetic'] != ARITHMETIC):
        raise ValueError('invalid Q8 request or explicit arithmetic contract')
    engine, name = request['engine'], request['tensor']
    if (not isinstance(engine, str) or engine not in config['models'] or
            not isinstance(name, str) or not 1 <= len(name) < 128 or
            not name.isascii() or any(ord(c) <= 32 or ord(c) == 127 for c in name)):
        raise ValueError('invalid Q8 model reference')
    k, n, m = (request[key] for key in ('k', 'n', 'm'))
    if (any(type(x) is not int for x in (k, n, m)) or
            not 1 <= k <= MAX_BYTES//4 or k % 32 or not 1 <= n <= MAX_BYTES//4 or
            not 1 <= m <= 32 or m*k*4 > MAX_BYTES or m*n*4 > MAX_BYTES or k*n//32*34 > MAX_WEIGHT_BYTES):
        raise ValueError('Q8 shape limit')
    encoded = request['input']
    if not isinstance(encoded, str) or len(encoded) != ((m*k*4+2)//3)*4:
        raise ValueError('Q8 input length')
    try:
        data = base64.b64decode(encoded, validate=True)
    except (ValueError, binascii.Error):
        raise ValueError('Q8 input encoding') from None
    if len(data) != m*k*4 or not finite_f32(data):
        raise ValueError('Q8 input must be finite little-endian float32')
    return engine, data


class Q8Broker:
    def __init__(self, config, environment):
        self.config, self.environment = config, environment
        self.policies = {}
        for engine, spec in config['models'].items():
            raw = Path(spec['policy']).read_bytes()
            if len(raw) > 16<<20 or hashlib.sha256(raw).hexdigest() != spec['policySha256']:
                raise ValueError('Q8 policy digest mismatch')
            policy = json.loads(raw)
            if policy['schema'] != 'shield-public-tensors/1' or policy['modelSha256'] != spec['modelSha256']:
                raise ValueError('Q8 policy model binding')
            entries = policy['tensors']
            if len(entries) > 100000 or len({x['name'] for x in entries}) != len(entries):
                raise ValueError('Q8 policy tensor limit')
            self.policies[engine] = {x['name']: x for x in entries}
        self.process = None
        self.directory = None
        self.engine = None
        self.retired = False
        self.requests = 0

    def close(self):
        process, self.process = self.process, None
        if process:
            if process.poll() is None:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
            process.wait()
            process.stdin.close()
            process.stdout.close()
        if self.directory:
            self.directory.cleanup()
            self.directory = None
        self.engine = None

    def _start(self, engine):
        # Planned turnover between completed requests, never retry a failed one.
        if self.process and self.requests >= MAX_EXECUTOR_REQUESTS:
            self.close()
        if self.process and self.engine == engine:
            if self.process.poll() is not None:
                raise Q8ExecutionError('executor exited')
            return
        self.close()
        self.directory = tempfile.TemporaryDirectory(dir=self.config['jobs'])
        spec = self.config['models'][engine]
        env = self.environment(spec, True, self.directory.name, engine)
        self.process = subprocess.Popen([self.config['sdk']+'/bin/shield-q8-executor', spec['model']],
            env=env, cwd=self.directory.name, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, start_new_session=True, bufsize=0)
        os.set_blocking(self.process.stdin.fileno(), False)
        os.set_blocking(self.process.stdout.fileno(), False)
        self.engine = engine
        self.requests = 0

    def _io(self, data, deadline, reading=False):
        fd = self.process.stdout.fileno() if reading else self.process.stdin.fileno()
        output = bytearray() if reading else memoryview(data)
        size = data if reading else len(data)
        offset = 0
        while offset < size:
            left = deadline-time.monotonic()
            if left <= 0:
                raise Q8ExecutionError('executor timeout')
            ready = select.select([fd] if reading else [], [] if reading else [fd], [], left)
            if not ready[0 if reading else 1]:
                raise Q8ExecutionError('executor timeout')
            try:
                chunk = os.read(fd, size-offset) if reading else os.write(fd, output[offset:])
            except (BlockingIOError, InterruptedError):
                continue
            if not chunk:
                raise Q8ExecutionError('executor pipe closed')
            if reading:
                output.extend(chunk)
                offset += len(chunk)
            else:
                offset += chunk
        return bytes(output) if reading else None

    def execute(self, request):
        if self.retired:
            raise Q8ExecutionError('Q8 executor retired')
        engine, data = validate_request(request, self.config)
        record = self.policies[engine].get(request['tensor'])
        if (record is None or record['type'] != 8 or
                record['ne'] != [request['k'], request['n'], 1, 1] or
                record['nbytes'] != request['k']*request['n']//32*34 or
                record['sha256'] != request['sha256']):
            raise ValueError('Q8 tensor is not an approved public reference')
        header = {key: request[key] for key in ('tensor','sha256','k','n','m','arithmetic')}
        header.update(schema='shield-q8-executor/1', inputBytes=len(data))
        try:
            self._start(engine)
            deadline = time.monotonic()+min(self.config['timeoutSeconds'],1100)
            self._io(json.dumps(header,separators=(',',':')).encode()+b'\n'+data, deadline)
            line = bytearray()
            while True:
                byte = self._io(1, deadline, True)
                if byte == b'\n':
                    break
                line.extend(byte)
                if len(line) > 8192:
                    raise Q8ExecutionError('executor response limit')
            reply = json.loads(line)
            if not isinstance(reply, dict) or reply.get('ok') is not True:
                raise Q8ExecutionError('executor refused computation')
            if reply.get('supported') is False:
                if reply != {'ok': True, 'supported': False, 'outputBytes': 0}:
                    raise Q8ExecutionError('invalid decline response')
                self.requests += 1
                return {'ok': True, 'supported': False}
            expected = request['m']*request['n']*4
            if (reply.get('supported') is not True or type(reply.get('outputBytes')) is not int or
                    reply['outputBytes'] != expected or reply.get('arithmetic') != ARITHMETIC):
                raise Q8ExecutionError('invalid executor response')
            stats = reply.get('stats')
            if (not isinstance(stats,dict) or set(stats) != set(STATS) or
                    any(type(stats[x]) is not int or not 0 <= stats[x] <= (1<<63)-1 for x in STATS) or
                    stats['verification_failures'] != 0 or stats['graph_macs'] != request['m']*request['k']*request['n'] or
                    stats['offloaded']+stats['local'] != 1 or
                    bool(stats['exchanges']) != bool(stats['macs']) or
                    (stats['local'] == 1 and stats['exchanges'])):
                raise Q8ExecutionError('invalid verified accounting')
            output = self._io(expected, deadline, True)
            if not finite_f32(output):
                raise Q8ExecutionError('non-finite executor output')
            self.requests += 1
            spec = self.config['models'][engine]
            return {'ok': True, 'supported': True, 'output': base64.b64encode(output).decode('ascii'),
                    'stats': stats, 'arithmetic': ARITHMETIC, 'modelSha256': spec['modelSha256'],
                    'policySha256': spec['policySha256'], 'tensorSha256': record['sha256']}
        except Exception:
            self.retired = True
            self.close()
            raise Q8ExecutionError('Q8 computation failed; no result released; executor retired') from None
