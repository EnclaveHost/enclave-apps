#!/usr/bin/env python3
"""Synthetic public-fixture integration test; requires a dedicated Shield worker."""
import argparse
import json
import numpy as np

from channel import MaskedChannel
from compiler import compile_graph, constant_digest, execute

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('--library', required=True)
p.add_argument('--host', default='127.0.0.1')
p.add_argument('--port', type=int, required=True)
a = p.parse_args()

rng = np.random.default_rng(71)
w = rng.integers(-2, 3, (512, 256), dtype=np.int64)
x = rng.integers(-100, 101, (4, 512), dtype=np.int64)
graph = {'schema': 'shield-graph/1', 'nodes': [
    {'name': 'input', 'op': 'input', 'shape': [4, 512], 'bound': 100},
    {'name': 'weights', 'op': 'constant', 'shape': [512, 256], 'value': w.tolist()},
    {'name': 'linear', 'op': 'matmul', 'args': ['input', 'weights']},
    {'name': 'activation', 'op': 'relu', 'args': ['linear']},
    {'name': 'square', 'op': 'multiply', 'args': ['activation', 'activation']},
], 'outputs': ['square']}
plan = compile_graph(graph, [constant_digest(w)])
channel = MaskedChannel(a.library, a.host, a.port, [constant_digest(w)])
result, routes = execute(plan, {'input': x}, channel)
reference = np.maximum(x @ w, 0) ** 2
np.testing.assert_array_equal(result['square'], reference)
if channel.stats['exchanges'] < 1 or channel.stats['verification_failures']:
    raise RuntimeError('test did not complete verified GPU offload')
print(json.dumps({'environment': 'host-side synthetic integration test; not enclave attestation',
                  'exactReferenceMatch': True, 'routes': routes, 'stats': channel.stats}, indent=2))
