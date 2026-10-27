"""Experimental integer graph compiler for Shield. This is not a CUDA driver.

The only remote instruction is Shield's existing verified FIELD_GEMM. There
is deliberately no generic upload, kernel launch, or opaque RPC instruction.
All shapes and range bounds are public compile-time metadata.
"""
from dataclasses import dataclass
import hashlib
import json
import math

import numpy as np

HALF_FIELD = (251 * 241 * 239) // 2
I64_MAX = (1 << 63) - 1
MAX_ELEMENTS = 16 * 1024 * 1024
# One FIELD_GEMM request remains limited to 64 rows. A graph may use a bounded
# sequence of these requests, determined solely by its public declared shape.
MASKED_TILE_ROWS = 64
MAX_MASKED_ROWS = 4096


class CompileError(ValueError):
    pass


def _shape(value):
    if (not isinstance(value, list) or not 1 <= len(value) <= 2 or
            any(type(n) is not int or n < 1 for n in value) or
            math.prod(value) > MAX_ELEMENTS):
        raise CompileError('invalid or oversized shape')
    return tuple(value)


def _bound(value):
    if type(value) is not int or not 0 <= value <= I64_MAX:
        raise CompileError('integer range cannot be represented without overflow')
    return value


def _array(value, shape, bound):
    # Reject floats, booleans and unsigned-to-signed wrap, rather than casting.
    a = np.array(value, copy=True)
    if a.dtype.kind not in 'iu' or a.shape != shape:
        raise ValueError('expected an integer tensor with the declared shape')
    if int(a.min()) < -bound or int(a.max()) > bound:
        raise ValueError('tensor exceeds its public range contract')
    return np.array(a, dtype='<i8', order='C', copy=True)


def constant_digest(value):
    a = np.asarray(value)
    shape = _shape(list(a.shape))
    a = _array(a, shape, I64_MAX)
    header = json.dumps({'dtype': 'i64', 'shape': list(shape)},
                        sort_keys=True, separators=(',', ':')).encode()
    return hashlib.sha256(header + b'\0' + a.tobytes()).hexdigest()


@dataclass(frozen=True)
class Instruction:
    name: str
    op: str
    args: tuple
    shape: tuple
    bound: int
    constant: bytes | None = None
    approved_public: bool = False
    placement: str = 'local'
    reason: str = 'trusted CPU implementation'


@dataclass(frozen=True)
class Plan:
    instructions: tuple
    outputs: tuple

    def explain(self):
        return {
            'schema': 'shield-compute-plan/1',
            'status': 'experimental; not CUDA-compatible',
            'arithmetic': 'exact bounded signed integers; no implicit quantization',
            'nodes': [{'name': n.name, 'op': n.op, 'shape': n.shape,
                       'placement': n.placement, 'reason': n.reason}
                      for n in self.instructions],
            'leakage': ['approved public weights', 'graph metadata and shapes',
                        'message sizes, timing and availability'],
        }


def compile_graph(graph, approved_public_constants=()):
    """Approval is supplied by trusted caller policy, never by graph labels.

    All runtime inputs remain private, including an input named "weight".
    Constants are private unless their exact typed content digest is approved.
    """
    if set(graph) != {'schema', 'nodes', 'outputs'} or graph['schema'] != 'shield-graph/1':
        raise CompileError('unsupported graph schema')
    if not isinstance(graph['nodes'], list) or not 1 <= len(graph['nodes']) <= 4096:
        raise CompileError('invalid graph size')
    approved = frozenset(approved_public_constants)
    seen = {}
    for node in graph['nodes']:
        if not isinstance(node, dict):
            raise CompileError('node must be an object')
        name, op = node.get('name'), node.get('op')
        if (not isinstance(name, str) or not name.isascii() or not name.isidentifier()
                or len(name) > 64 or name in seen):
            raise CompileError('invalid or duplicate node name')
        if op == 'input':
            if set(node) != {'name', 'op', 'shape', 'bound'}:
                raise CompileError('input has unsupported fields')
            n = Instruction(name, op, (), _shape(node['shape']), _bound(node['bound']))
        elif op == 'constant':
            if set(node) != {'name', 'op', 'shape', 'value'}:
                raise CompileError('constant has unsupported fields')
            shape = _shape(node['shape'])
            a = _array(node['value'], shape, I64_MAX)
            n = Instruction(name, op, (), shape, max(abs(int(a.min())), abs(int(a.max()))),
                            a.tobytes(), constant_digest(a) in approved)
        else:
            arities = {'matmul': 2, 'add': 2, 'multiply': 2, 'relu': 1, 'transpose': 1}
            if op not in arities:
                raise CompileError('unsupported operation; opaque kernels and raw GPU calls are refused')
            if set(node) != {'name', 'op', 'args'}:
                raise CompileError('operation has unsupported fields')
            args = node['args']
            if (not isinstance(args, list) or len(args) != arities[op] or
                    any(not isinstance(a, str) or a not in seen for a in args)):
                raise CompileError('invalid operands or forward reference')
            a = seen[args[0]]
            b = seen[args[-1]]
            shape, bound = a.shape, a.bound
            place, reason = 'local', 'trusted CPU implementation'
            if op == 'matmul':
                if len(a.shape) != 2 or len(b.shape) != 2 or a.shape[1] != b.shape[0]:
                    raise CompileError('matmul shape mismatch')
                shape = (a.shape[0], b.shape[1])
                bound = a.bound * b.bound * a.shape[1]
                if (b.constant is not None and b.approved_public and b.bound <= 119 and
                        a.shape[1] % 32 == 0 and a.shape[0] <= MAX_MASKED_ROWS and
                        a.bound <= HALF_FIELD and bound <= HALF_FIELD):
                    place, reason = 'masked-field-gemm', 'approved public int8 weights; static no-wrap bound'
                else:
                    reason = 'no approved masked lowering for these operands, shape or range'
            elif op in ('add', 'multiply'):
                if a.shape != b.shape:
                    raise CompileError('elementwise shape mismatch; broadcasting is not implicit')
                bound = a.bound + b.bound if op == 'add' else a.bound * b.bound
            elif op == 'transpose':
                if len(a.shape) != 2:
                    raise CompileError('transpose requires a matrix')
                shape = tuple(reversed(a.shape))
            _shape(list(shape))
            n = Instruction(name, op, tuple(args), shape, _bound(bound), placement=place, reason=reason)
        seen[name] = n
    outputs = graph['outputs']
    if (not isinstance(outputs, list) or not outputs or
            any(not isinstance(x, str) or x not in seen for x in outputs)):
        raise CompileError('invalid outputs')
    return Plan(tuple(seen.values()), tuple(outputs))


def _execute(plan, inputs, channel=None):
    """Run inside the trusted VM. On a channel error, publish no graph outputs.

    A channel is created per execution, carries only preapproved constants and
    masked field operations, and is closed on success or failure. Offline CPU
    execution is selected explicitly by omitting the channel before execution.
    """
    required = {n.name for n in plan.instructions if n.op == 'input'}
    if set(inputs) != required:
        raise ValueError('inputs do not match the compiled graph')
    values = {}
    # Validate every input before opening any connection. No secret-dependent
    # choice between masked offload and fallback occurs during execution.
    for n in plan.instructions:
        if n.op == 'input':
            values[n.name] = _array(inputs[n.name], n.shape, n.bound)
        elif n.op == 'constant':
            values[n.name] = np.frombuffer(n.constant, dtype='<i8').reshape(n.shape)
    routes = {}
    remote = {}
    if channel is not None:
        for n in plan.instructions:
            if n.placement == 'masked-field-gemm':
                remote[n.name] = channel.register(values[n.args[1]], min(n.shape[0], MASKED_TILE_ROWS))
        if remote:
            channel.start()
    for n in plan.instructions:
        if n.op in ('input', 'constant'):
            continue
        args = [values[a] for a in n.args]
        if n.name in remote:
            # Keep the entire result private until every tile has passed the
            # existing channel verification. Never fall back after an offload
            # begins, and wipe staged rows on a later-tile failure.
            staged = np.empty(n.shape, dtype=np.int64)
            try:
                for first in range(0, n.shape[0], MASKED_TILE_ROWS):
                    last = min(first + MASKED_TILE_ROWS, n.shape[0])
                    tile = channel.matmul(remote[n.name], args[0][first:last])
                    if tile.shape != (last-first, n.shape[1]) or tile.dtype != np.dtype('int64'):
                        raise RuntimeError('invalid verified tile result')
                    staged[first:last] = tile
                values[n.name] = staged
            except BaseException:
                staged.fill(0)
                raise
            routes[n.name] = 'masked-field-gemm'
        else:
            if n.op == 'matmul': result = args[0] @ args[1]
            elif n.op == 'add': result = args[0] + args[1]
            elif n.op == 'multiply': result = args[0] * args[1]
            elif n.op == 'relu': result = np.maximum(args[0], 0)
            elif n.op == 'transpose': result = args[0].T.copy()
            else: raise RuntimeError('invalid compiled instruction')
            values[n.name] = result
            routes[n.name] = 'local'
    return {name: values[name].copy() for name in plan.outputs}, routes


def execute(plan, inputs, channel=None):
    try:
        return _execute(plan, inputs, channel)
    finally:
        # Input-contract failures also close a caller-created channel.
        if channel is not None:
            channel.close()
