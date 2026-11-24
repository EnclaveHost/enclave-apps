#!/usr/bin/python3
"""Guest-local compute boundary. Configuration is part of the measured image.

Clients cannot supply executable paths, environment variables, worker addresses,
weight approvals or backend choices. The service account alone can reach the
fixed worker endpoint. Plaintext requests and results stay on the guest socket.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import socket
import socketserver
import struct
import subprocess
import sys
import tempfile
import threading
import time

# Isolated Python startup ignores ambient search paths, including script dir.
sys.path.insert(0, str(Path(__file__).resolve().parent))

MAX_REQUEST = 8 << 20
MAX_RESPONSE = 16 << 20
MAX_NATIVE_FILE = 8 << 20
# The largest file a child may ask for by raising its own limit, which only the Q8 executor does: its
# session memory is a memfd of four 64 MiB regions, and an arena is a memfd of up to a gigabyte
# (native/q8_executor.cpp, kMaxArena). A file a child writes without asking is still bound at
# MAX_NATIVE_FILE.
MAX_SHARED_FILE = 1 << 30
NATIVE_ARITHMETIC = 'native-model-v1'
FIXED_ARITHMETIC = 'shield-fixed-point-v1'
STATS = re.compile(r'\[enclave-shield\] offloaded=(\d+) local=(\d+) graph_macs=(\d+) exchanges=(\d+) macs=(\d+) verification_failures=(\d+)')


def parse_native_stats(stderr):
    matches = STATS.findall(stderr)
    return dict(zip(('offloaded', 'local', 'graph_macs', 'exchanges', 'macs',
                     'verification_failures'), map(int, matches[-1]))) if matches else {}


def receive(sock, limit, deadline=None):
    def exact(n):
        data = bytearray()
        while len(data) < n:
            if deadline is not None:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("request framing deadline")
                sock.settimeout(remaining)
            chunk = sock.recv(n - len(data))
            if not chunk:
                raise ValueError('truncated request')
            data.extend(chunk)
        return bytes(data)
    size, = struct.unpack('!I', exact(4))
    if not 1 <= size <= limit:
        raise ValueError('message size limit')
    return json.loads(exact(size))


def send(sock, value):
    data = json.dumps(value, separators=(',', ':')).encode()
    if len(data) > MAX_RESPONSE:
        raise ValueError('response size limit')
    sock.sendall(struct.pack('!I', len(data)) + data)


def verify_file(path, expected):
    with open(path, 'rb') as f:
        actual = hashlib.file_digest(f, 'sha256').hexdigest()
    if actual != expected:
        raise ValueError('measured artifact digest mismatch')



# Administrator's settings for the link to the workers (config "link"). The defaults above are for a small
# guest and one worker over TCP; on a host with processors to spare the same route measured twice as fast
# with a ring to each worker and a refill thread per pool (opencl/Q8_DIRECT.md, "EyesOff's settings").
# Every value is checked here: nothing in it is passed through as text.
LINK_NUMBERS = {'refillThreads': ('SHIELDED_REFILL_THREADS', 1, 64), 'maxRows': ('SHIELDED_MAX_M', 1, 64),
                'poolDepth': ('SHIELDED_POOL_DEPTH', 1, 256), 'refillBatch': ('SHIELDED_REFILL_BATCH', 1, 256)}
LINK_SWITCHES = {'splitColumns': 'SHIELDED_SPLIT_COLS', 'overlapVerify': 'SHIELDED_OVERLAP_VERIFY'}
RING_DIRECTORY = '/dev/enclave-shielded-shm/'


def link_environment(link):
    if link is None:
        return {}
    if not isinstance(link, dict) or set(link) - set(LINK_NUMBERS) - set(LINK_SWITCHES) - {'workers'}:
        raise ValueError('invalid link settings')
    env = {}
    for key, (name, least, most) in LINK_NUMBERS.items():
        if key in link:
            value = link[key]
            if type(value) is not int or not least <= value <= most:
                raise ValueError('invalid link setting ' + key)
            env[name] = str(value)
    for key, name in LINK_SWITCHES.items():
        if key in link:
            if type(link[key]) is not bool:
                raise ValueError('invalid link setting ' + key)
            if link[key]:
                env[name] = '1'
    if 'workers' in link:
        workers = link['workers']
        if not isinstance(workers, list) or not 1 <= len(workers) <= 8:
            raise ValueError('invalid link workers')
        lines = []
        for worker in workers:
            if not isinstance(worker, dict) or set(worker) - {'host', 'port', 'reserveBytes', 'ring', 'ringBytes'}:
                raise ValueError('invalid link worker')
            host, port, reserve = worker.get('host'), worker.get('port'), worker.get('reserveBytes')
            if (not isinstance(host, str) or not re.fullmatch(r'[0-9A-Za-z.:-]{1,64}', host) or
                    type(port) is not int or not 1 <= port <= 65535 or
                    type(reserve) is not int or not 1 <= reserve <= 1 << 40):
                raise ValueError('invalid link worker')
            line = '%s|%d|0|%d' % (host, port, reserve)
            if 'ring' in worker or 'ringBytes' in worker:
                ring, size = worker.get('ring'), worker.get('ringBytes', 64 << 20)
                if (not isinstance(ring, str) or not ring.startswith(RING_DIRECTORY) or
                        not re.fullmatch(r'[0-9A-Za-z._-]{1,64}', ring[len(RING_DIRECTORY):]) or
                        type(size) is not int or size not in (8 << 20, 16 << 20, 32 << 20, 64 << 20)):
                    raise ValueError('invalid link worker ring')
                line += '|%s|%d' % (ring, size)
            lines.append(line)
        env['SHIELDED_WORKERS'] = '\n'.join(lines)
    return env


# How a session is served, as the executor's environment. Experimental and off unless the policy sets it;
# measured in opencl/Q8_DIRECT.md. `mailbox` lets a client and the executor pass requests through the
# session's shared memory without a system call; `serveProcessor` is the hardware thread the serving
# thread stays on (the client's requesting thread belongs on the other one of the same core, which the
# desktop's environment sets); `continueInline` has that thread compute a feed-forward block's
# continuation itself; `refillNice` lowers the priority of the pad refill threads.
ROUTE_SWITCHES = {'mailbox': 'SHIELD_Q8_MAILBOX', 'continueInline': 'SHIELD_Q8_AHEAD_INLINE'}
ROUTE_NUMBERS = {'serveProcessor': ('SHIELD_Q8_SERVE_CPU', 0, 1023), 'refillNice': ('SHIELD_Q8_NICE_REFILL', 1, 19)}


def route_environment(route):
    if route is None:
        return {}
    if not isinstance(route, dict) or set(route) - set(ROUTE_SWITCHES) - set(ROUTE_NUMBERS):
        raise ValueError('invalid route settings')
    env = {}
    for key, name in ROUTE_SWITCHES.items():
        if key in route:
            if type(route[key]) is not bool:
                raise ValueError('invalid route setting ' + key)
            if route[key]:
                env[name] = '1'
    for key, (name, least, most) in ROUTE_NUMBERS.items():
        if key in route:
            value = route[key]
            if type(value) is not int or not least <= value <= most:
                raise ValueError('invalid route setting ' + key)
            env[name] = str(value)
    if ('serveProcessor' in route or route.get('continueInline')) and not route.get('mailbox'):
        raise ValueError('route settings need the mailbox')
    return env


class Compute:
    def __init__(self, config):
        if config['schema'] != 'shield-guest-service/1':
            raise ValueError('unsupported service policy')
        self.config = config
        self.lock = threading.Lock()
        self.retired = False
        self.graph_failure = None
        self.completed = 0
        self.active = None
        self.telemetry = {'verifiedOffloads': 0, 'maskedMacs': 0, 'lastCompleted': None}
        self.calibration_digests = {}
        self.q8_broker = None
        self.q8_lock = threading.Lock()
        for engine, spec in config['models'].items():
            # Deployment mounts this filesystem read-only with dm-verity.
            verify_file(spec['model'], spec['modelSha256'])
            verify_file(spec['policy'], spec['policySha256'])
            policy = json.loads(Path(spec['policy']).read_text())
            if policy['modelSha256'] != spec['modelSha256']:
                raise ValueError('model/policy mismatch')
            verify_file(spec['calibration'], policy['calibrationSha256'])
            self.calibration_digests[engine] = policy['calibrationSha256']

    def collect_q8(self):
        # Session clients talk to the executor directly, so their verified
        # work is read back from it here rather than recorded per request.
        broker = self.q8_broker
        if broker is None:
            return
        from q8_service import Q8ExecutionError
        with self.q8_lock:
            try:
                exchanges, macs, products = broker.collect()
            except Q8ExecutionError:
                self.retired = True
                return
            if broker.retired:
                self.retired = True
            if products:
                previous = self.telemetry
                self.telemetry = {
                    'verifiedOffloads': previous['verifiedOffloads'] + exchanges,
                    'maskedMacs': previous['maskedMacs'] + macs,
                    'lastCompleted': {'engine': 'q8', 'mode': 'masked',
                                      'verifiedOffloads': exchanges, 'maskedMacs': macs}}
                self.completed += products

    def status(self):
        self.collect_q8()
        active = self.active
        return {'ok': True, 'schema': 'shield-guest-status/1',
                'state': 'retired' if self.retired else 'busy' if self.lock.locked() else 'ready',
                'active': {**active, 'elapsedSeconds': round(time.monotonic() - active['started'])}
                    if active else None,
                'completed': self.completed, 'engines': sorted(self.config['models']),
                'gpuRoute': 'masked-verified-only', 'fallback': 'trusted guest CPU',
                'nativeArithmetic': {'cpu': NATIVE_ARITHMETIC, 'masked': FIXED_ARITHMETIC,
                                     'maskedRequiresExplicitOptIn': True},
                'cudaBinaryCompatibility': False, 'telemetry': self.telemetry,
                'graphFailure': dict(self.graph_failure) if self.graph_failure else None}

    def record_completed(self, engine, mode, stats):
        # Only trusted execution results contribute counters. Never include
        # prompts, tensors, filenames or caller-provided metrics in status.
        offloads = stats.get('exchanges', 0) if mode == 'masked' else 0
        macs = stats.get('macs', 0) if offloads else 0
        previous = self.telemetry
        self.telemetry = {
            'verifiedOffloads': previous['verifiedOffloads'] + offloads,
            'maskedMacs': previous['maskedMacs'] + macs,
            'lastCompleted': {'engine': engine, 'mode': mode,
                              'verifiedOffloads': offloads, 'maskedMacs': macs}}
        self.completed += 1

    def environment(self, spec, masked, directory, engine):
        root = self.config['sdk']
        env = {'PATH': '/usr/bin:/bin', 'HOME': directory, 'TMPDIR': directory,
               'LANG': 'C.UTF-8', 'OMP_NUM_THREADS': str(self.config['threads']),
               'GGML_BACKEND_PATH': root + '/lib/backends/libggml-cpu.so'}
        # An engine's idle OpenMP thread spins while the engine waits for the
        # worker. On a guest with two processors that takes one of them from
        # the thread that makes the pads, and generation runs at about half
        # speed. With processors to spare, spinning is slightly faster.
        if len(os.sched_getaffinity(0)) < 4:
            env['OMP_WAIT_POLICY'] = 'passive'
        if masked:
            env.update({
                'ENCLAVE_SHIELD_POLICY': spec['policy'],
                'ENCLAVE_SHIELD_POLICY_SHA256': spec['policySha256'],
                'ENCLAVE_SHIELD_ARITHMETIC': FIXED_ARITHMETIC,
                'ENCLAVE_SHIELD_CPU': root + '/lib/backends/libggml-cpu.so',
                'ENCLAVE_SHIELD_BACKEND': root + '/lib/backends/libggml-shielded.so',
                'LD_PRELOAD': root + '/lib/libenclave-shield-bootstrap.so',
                'SHIELDED_CALIB': spec['calibration'],
                'SHIELDED_HOST': self.config['worker']['host'],
                'SHIELDED_PORT': str(self.config['worker']['port']),
                'SHIELDED_RESERVE_BYTES': str(spec['reserveBytes']),
                'SHIELDED_REFILL_THREADS': '1', 'SHIELDED_MAX_M': '32',
                'SHIELDED_POOL_DEPTH': '32',
            })
            env.update(link_environment(self.config.get('link')))
            # The mailbox has the client and the serving thread wait by spinning. With fewer than four
            # processors that takes them from the threads that make the pads, as an idle OpenMP thread
            # does above, so the route settings are left out and sessions use the socket.
            if len(os.sched_getaffinity(0)) >= 4:
                env.update(route_environment(self.config.get('route')))
            else:
                route_environment(self.config.get('route'))  # still validated
            # The executor computes a session product itself when the pad pool
            # has run dry and that is cheaper than an exchange that makes its
            # own pad (native/q8_executor.cpp, PadStock). With this set every
            # product is exchanged, and equal requests give equal bytes.
            if self.config.get('q8ExchangeEveryProduct') is True:
                env['SHIELD_Q8_DRY_LOCAL'] = '0'
            if engine == 'sd':
                env['SD_ACCEL_BACKEND'] = root + '/lib/backends/libggml-shielded.so'
                # Image-token batches exceed MAX_M. Split them into bounded,
                # independently masked and verified exchanges in the C link.
                env['SHIELDED_WIDE'] = '1'
        return env

    def run(self, request):
        if not isinstance(request, dict):
            raise ValueError('expected request object')
        if request == {'op': 'status'}:
            return self.status()
        if not self.lock.acquire(blocking=False):
            raise ValueError('compute busy')
        try:
            if self.retired:
                raise ValueError('compute retired; administrator restart required')
            if request.get('op') == 'q8_0_matmul':
                return self.q8(request)
            # A native/graph job owns the worker reservation while it runs.
            if self.q8_broker is not None:
                with self.q8_lock:
                    self.q8_broker.close()
                self.collect_q8()
            if request.get('op') == 'graph':
                return self.graph(request)
            return self.native(request)
        finally:
            self.active = None
            self.lock.release()

    def q8(self, request):
        if self.config.get('enableQ8Experimental') is not True:
            raise ValueError('experimental Q8 execution is disabled')
        from q8_service import Q8Broker, Q8ExecutionError
        if self.q8_broker is None:
            self.q8_broker = Q8Broker(self.config, self.environment)
        self.active = {'engine': 'q8', 'mode': 'masked', 'started': time.monotonic()}
        try:
            with self.q8_lock:
                result = self.q8_broker.execute(request)
            if result.get('supported'):
                self.record_completed('q8', 'masked', result['stats'])
            return result
        except Q8ExecutionError:
            self.retired = True
            raise ValueError('Q8 computation failed; compute retired; no result released') from None

    def q8_session(self, request, client):
        """Attach a client's socket to the Q8 executor for direct binary requests.

        The caller has checked the peer's credentials. Returns the reply to
        send; after an 'ok' the socket belongs to the executor's session.
        """
        from q8_service import ARITHMETIC, Q8Broker, Q8ExecutionError
        if (not isinstance(request, dict) or set(request) != {'op', 'engine', 'arithmetic'} or
                request['arithmetic'] != ARITHMETIC or not isinstance(request['engine'], str) or
                request['engine'] not in self.config['models']):
            raise ValueError('invalid Q8 session or explicit arithmetic contract')
        if self.config.get('enableQ8Experimental') is not True:
            raise ValueError('experimental Q8 execution is disabled')
        if not self.lock.acquire(blocking=False):
            raise ValueError('compute busy')
        try:
            if self.retired:
                raise ValueError('compute retired; administrator restart required')
            if self.q8_broker is None:
                self.q8_broker = Q8Broker(self.config, self.environment)
            try:
                with self.q8_lock:
                    attached = self.q8_broker.attach(request['engine'], client)
            except Q8ExecutionError:
                self.retired = True
                raise ValueError('Q8 computation failed; compute retired; no result released') from None
            if not attached:
                raise ValueError('compute busy')
            return {'ok': True, 'session': 'shield-q8-session/1'}
        finally:
            self.lock.release()

    def q8_warm(self, request):
        """A client expects to use Q8 soon: start the executor it used last.

        A hint only. It starts no computation, and a wrong guess costs the
        later session one executor restart.
        """
        from q8_service import ARITHMETIC, Q8Broker
        if (not isinstance(request, dict) or set(request) != {'op', 'arithmetic'} or
                request['arithmetic'] != ARITHMETIC):
            raise ValueError('invalid Q8 hint or explicit arithmetic contract')
        if self.config.get('enableQ8Experimental') is not True:
            raise ValueError('experimental Q8 execution is disabled')
        if not self.lock.acquire(blocking=False):
            return {'ok': True, 'warm': False}
        try:
            if self.retired:
                raise ValueError('compute retired; administrator restart required')
            if self.q8_broker is None:
                self.q8_broker = Q8Broker(self.config, self.environment)
            with self.q8_lock:
                self.q8_broker.warm(self.q8_broker.last_engine)
            return {'ok': True, 'warm': True}
        finally:
            self.lock.release()

    def graph(self, request):
        if set(request) != {'op', 'graph', 'inputs', 'mode'} or request['mode'] not in ('masked', 'cpu'):
            raise ValueError('invalid graph request')
        from compiler import compile_graph, execute
        from channel import MaskedChannel, ChannelError
        approved = self.config['publicConstants']
        plan = compile_graph(request['graph'], approved)
        # Bound aggregate memory, including intermediates, independently of
        # each tensor's compiler limit. Never accept policy from the client.
        if sum(__import__('math').prod(n.shape) for n in plan.instructions) > 8 * 1024 * 1024:
            raise ValueError('graph memory limit')
        shapes = {n.name: n.shape for n in plan.instructions}
        if sum(n.shape[0] * n.shape[1] * shapes[n.args[0]][1]
               for n in plan.instructions if n.op == 'matmul') > 100_000_000:
            raise ValueError('graph work limit')
        if sum(__import__('math').prod(shapes[n]) for n in plan.outputs) > 200_000:
            raise ValueError('graph output limit')
        channel = None
        self.active = {'engine': 'graph', 'mode': request['mode'], 'started': time.monotonic()}
        try:
            if request['mode'] == 'masked' and any(n.placement == 'masked-field-gemm' for n in plan.instructions):
                channel = MaskedChannel(self.config['core'], self.config['worker']['host'],
                                        self.config['worker']['port'], approved)
            outputs, routes = execute(plan, request['inputs'], channel)
            result = {'ok': True, 'outputs': {k: v.tolist() for k, v in outputs.items()},
                      'routes': routes, 'stats': channel.stats if channel else {}}
            if len(json.dumps(result)) > MAX_RESPONSE:
                raise ValueError('graph output limit')
            self.record_completed('graph', request['mode'], result['stats'])
            return result
        except ChannelError as error:
            self.graph_failure = error.public_failure()
            self.retired = True
            raise ValueError('masked computation failed; no result released') from None

    def native(self, request):
        if set(request) - {'op', 'prompt', 'mode', 'seed', 'tokens', 'arithmetic'}:
            raise ValueError('unsupported request fields')
        engine = request.get('op')
        if engine not in ('llama', 'sd') or engine not in self.config['models']:
            raise ValueError('unsupported engine')
        prompt = request.get('prompt')
        if not isinstance(prompt, str) or not 1 <= len(prompt.encode()) <= 8192 or '\0' in prompt:
            raise ValueError('invalid prompt')
        mode = request.get('mode', 'masked')
        if mode not in ('masked', 'cpu'):
            raise ValueError('unsupported compute mode')
        seed, tokens = request.get('seed', 42), request.get('tokens', 128)
        if type(seed) is not int or not 0 <= seed <= 0x7fffffff:
            raise ValueError('invalid seed')
        if type(tokens) is not int or not 1 <= tokens <= 256 or (engine == 'sd' and 'tokens' in request):
            raise ValueError('invalid token limit')
        arithmetic = request.get('arithmetic', NATIVE_ARITHMETIC if mode == 'cpu' else None)
        expected = FIXED_ARITHMETIC if mode == 'masked' else NATIVE_ARITHMETIC
        if arithmetic != expected:
            if mode == 'masked':
                raise ValueError(f'native masked mode requires arithmetic="{FIXED_ARITHMETIC}"; '
                                 'results may differ from CPU reference')
            raise ValueError('CPU mode requires native-model-v1 arithmetic')
        spec, root = self.config['models'][engine], self.config['sdk']
        self.active = {'engine': engine, 'mode': mode, 'started': time.monotonic()}
        with tempfile.TemporaryDirectory(dir=self.config['jobs']) as directory:
            d = Path(directory)
            (d / 'prompt').write_text(prompt)
            command = [root + '/bin/' + ('llama-cli' if engine == 'llama' else 'sd-cli'),
                       '-m', spec['model']]
            if engine == 'llama':
                command += ['-f', str(d / 'prompt'), '-n', str(tokens), '-c', '1024',
                            '-b', '32', '-ub', '32', '-t', str(self.config['threads']),
                            '--temp', '0', '--seed', str(seed), '--no-conversation',
                            '--single-turn', '--no-warmup', '--no-display-prompt', '--color', 'off']
            else:
                command += ['--prompt-file', str(d / 'prompt'), '-o', str(d / 'result.png'),
                            '--width', '256', '--height', '256', '--steps', '8', '--cfg-scale', '7',
                            '--sampling-method', 'euler', '--seed', str(seed),
                            '--threads', str(self.config['threads']), '--backend', 'cpu', '--mmap', '--diffusion-fa']
            env = self.environment(spec, mode == 'masked', directory, engine)
            with (d / 'stdout').open('wb') as out, (d / 'stderr').open('wb') as err:
                process = subprocess.Popen(command, env=env, cwd=directory, stdin=subprocess.DEVNULL,
                                           stdout=out, stderr=err, start_new_session=True)
                try:
                    process.wait(timeout=self.config['timeoutSeconds'])
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
                    self.retired = True
                    raise ValueError('compute timeout; no result released') from None
            stderr = (d / 'stderr').read_text(errors='replace')
            stats = parse_native_stats(stderr)
            if process.returncode or (mode == 'masked' and
                    (not stats or stats['verification_failures'])):
                self.retired = True
                raise ValueError('compute failed; no result released')
            contract = {'id': arithmetic, 'approximate': mode == 'masked',
                        'modelSha256': spec['modelSha256']}
            if mode == 'masked':
                contract.update(policySha256=spec['policySha256'],
                                calibrationSha256=self.calibration_digests[engine])
            result = {'ok': True, 'mode': mode, 'arithmetic': contract, 'stats': stats}
            if engine == 'llama':
                result['text'] = (d / 'stdout').read_text(errors='replace')
            else:
                result['png'] = base64.b64encode((d / 'result.png').read_bytes()).decode('ascii')
            self.record_completed(engine, mode, stats)
            return result


class Handler(socketserver.BaseRequestHandler):
    def handle(self):
        # The socket ACL and SO_PEERCRED are both checked; no host port listens.
        _, uid, _ = struct.unpack('3i', self.request.getsockopt(socket.SOL_SOCKET, socket.SO_PEERCRED, 12))
        if uid not in self.server.compute.config['clientUids']:
            return
        self.request.settimeout(self.server.frame_timeout)
        try:
            request = receive(self.request, MAX_REQUEST,
                              deadline=time.monotonic() + self.server.frame_timeout)
        except (OSError, ValueError, RecursionError):
            # A malformed or stalled client has not started protected work.
            # It must not retire the shared compute service.
            try:
                send(self.request, {'ok': False, 'error': 'invalid request frame'})
            except (OSError, ValueError):
                pass
            return
        self.request.settimeout(self.server.frame_timeout)
        try:
            if isinstance(request, dict) and request.get('op') == 'q8_session':
                result = self.server.compute.q8_session(request, self.request)
                # The executor now owns this connection. Answer, then let go of
                # this copy without shutting the shared socket down.
                try:
                    send(self.request, result)
                except (OSError, ValueError):
                    pass
                os.close(self.request.detach())
                return
            if isinstance(request, dict) and request.get('op') == 'q8_warm':
                result = self.server.compute.q8_warm(request)
            else:
                result = self.server.compute.run(request)
        except (ValueError, KeyError, TypeError, OverflowError) as error:
            result = {'ok': False, 'error': str(error)[:200]}
        except Exception:
            self.server.compute.retired = True
            result = {'ok': False, 'error': 'internal failure; compute retired'}
        try:
            send(self.request, result)
        except (OSError, ValueError):
            pass


class Server(socketserver.ThreadingMixIn, socketserver.UnixStreamServer):
    daemon_threads = True
    request_queue_size = 32
    max_clients = 16
    frame_timeout = 10

    def __init__(self, *args, **kwargs):
        # Capacity belongs to this server, not every instance in the process.
        self.slots = threading.BoundedSemaphore(self.max_clients)
        super().__init__(*args, **kwargs)

    def process_request(self, request, address):
        # Apply bounded transport backpressure before creating another thread.
        # Closing a just-accepted socket here loses requests, especially when a
        # client is still writing a large frame. Existing handlers can release
        # slots independently; their framing reads have a finite timeout.
        self.slots.acquire()
        try:
            super().process_request(request, address)
        except BaseException:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--config', type=Path, required=True)
    a = p.parse_args()
    # Reject diagnostics, alternate libraries and pad sources inherited from a
    # parent. Each native child additionally gets an allowlisted environment.
    os.environ.clear()
    os.umask(0o077)
    # Set once before serving threads; children inherit the bound without an
    # unsafe preexec_fn in this multithreaded process. Never raise an outer cap.
    # The soft limit is what a child's write meets. The hard limit is how far a child may raise its own,
    # and is the size of the executor's largest shared memory.
    _, hard = resource.getrlimit(resource.RLIMIT_FSIZE)
    bound = MAX_NATIVE_FILE if hard == resource.RLIM_INFINITY else min(MAX_NATIVE_FILE, hard)
    ceiling = MAX_SHARED_FILE if hard == resource.RLIM_INFINITY else min(MAX_SHARED_FILE, hard)
    resource.setrlimit(resource.RLIMIT_FSIZE, (bound, max(bound, ceiling)))
    compute = Compute(json.loads(a.config.read_text()))
    path = Path(compute.config['socket'])
    path.unlink(missing_ok=True)
    with Server(str(path), Handler) as server:
        server.compute = compute
        os.chmod(path, 0o660)
        print('Shield guest service ready', flush=True)
        server.serve_forever()


if __name__ == '__main__':
    main()
