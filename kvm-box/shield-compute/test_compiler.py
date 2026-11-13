import copy
import unittest

import numpy as np

from compiler import CompileError, HALF_FIELD, MAX_MASKED_ROWS, compile_graph, constant_digest, execute


def example():
    w = np.arange(32 * 8, dtype=np.int64).reshape(32, 8) % 5 - 2
    graph = {'schema': 'shield-graph/1', 'nodes': [
        {'name': 'input', 'op': 'input', 'shape': [2, 32], 'bound': 10},
        {'name': 'weights', 'op': 'constant', 'shape': [32, 8], 'value': w.tolist()},
        {'name': 'linear', 'op': 'matmul', 'args': ['input', 'weights']},
        {'name': 'activation', 'op': 'relu', 'args': ['linear']},
        {'name': 'square', 'op': 'multiply', 'args': ['activation', 'activation']},
    ], 'outputs': ['square']}
    return graph, w


class RecordingChannel:
    def __init__(self, corrupt=False):
        self.weights = []
        self.started = False
        self.closed = False
        self.corrupt = corrupt
        self.rows = []
        self.calls = []

    def register(self, weight, rows):
        self.rows.append(rows)
        self.weights.append(weight.copy())
        return len(self.weights) - 1

    def start(self):
        self.started = True

    def matmul(self, node, x):
        self.calls.append((node, x.shape[0]))
        if self.corrupt:
            raise RuntimeError('verified transport rejected forged result')
        return x @ self.weights[node]

    def close(self):
        self.closed = True


class CompilerTests(unittest.TestCase):
    def setUp(self):
        self.graph, self.w = example()
        self.x = np.arange(64).reshape(2, 32) % 21 - 10
        self.approved = [constant_digest(self.w)]

    def test_mixed_graph_matches_cpu_and_only_linear_crosses_boundary(self):
        plan = compile_graph(self.graph, self.approved)
        transport = RecordingChannel()
        result, routes = execute(plan, {'input': self.x}, transport)
        cpu, _ = execute(plan, {'input': self.x})
        np.testing.assert_array_equal(result['square'], cpu['square'])
        self.assertEqual(routes, {'linear': 'masked-field-gemm', 'activation': 'local', 'square': 'local'})
        self.assertEqual(len(transport.weights), 1)
        self.assertTrue(transport.closed)

    def test_unapproved_and_substituted_constants_remain_local(self):
        for approved in ([], self.approved):
            graph = copy.deepcopy(self.graph)
            graph['nodes'][1]['value'][0][0] += 1
            transport = RecordingChannel()
            execute(compile_graph(graph, approved), {'input': self.x}, transport)
            self.assertFalse(transport.started)
            self.assertEqual(transport.weights, [])

    def test_private_matrix_named_weight_never_uploaded(self):
        graph = copy.deepcopy(self.graph)
        graph['nodes'][1] = {'name': 'weights', 'op': 'input', 'shape': [32, 8], 'bound': 2}
        channel = RecordingChannel()
        execute(compile_graph(graph, self.approved), {'input': self.x, 'weights': self.w}, channel)
        self.assertFalse(channel.started)

    def test_graph_cannot_declassify_or_request_raw_gpu(self):
        for op in ('cudaLaunchKernel', 'ptx', 'memcpy_host_to_gpu', 'rpc'):
            graph = copy.deepcopy(self.graph)
            graph['nodes'][2]['op'] = op
            with self.assertRaises(CompileError): compile_graph(graph, self.approved)
        graph = copy.deepcopy(self.graph)
        graph['nodes'][0]['public'] = True
        with self.assertRaises(CompileError): compile_graph(graph, self.approved)

    def test_field_overflow_stays_local_integer_overflow_is_rejected(self):
        graph = copy.deepcopy(self.graph)
        graph['nodes'] = graph['nodes'][:3]
        graph['outputs'] = ['linear']
        graph['nodes'][0]['bound'] = HALF_FIELD
        self.assertEqual(compile_graph(graph, self.approved).instructions[-1].placement, 'local')
        graph['nodes'][0]['bound'] = 1 << 62
        with self.assertRaises(CompileError): compile_graph(graph, self.approved)

    def test_out_of_range_inputs_fail_before_any_upload(self):
        channel = RecordingChannel()
        bad = self.x.copy()
        bad[0, 0] = 11
        with self.assertRaises(ValueError):
            execute(compile_graph(self.graph, self.approved), {'input': bad}, channel)
        self.assertEqual(channel.weights, [])
        self.assertFalse(channel.started)
        self.assertTrue(channel.closed)

    def test_forgery_does_not_retry_locally_or_release_outputs(self):
        channel = RecordingChannel(corrupt=True)
        with self.assertRaisesRegex(RuntimeError, 'forged'):
            execute(compile_graph(self.graph, self.approved), {'input': self.x}, channel)
        self.assertTrue(channel.closed)

    def test_float_and_wrapping_casts_are_not_implicit(self):
        plan = compile_graph(self.graph, self.approved)
        for bad in (self.x.astype(float), np.full((2, 32), 2**64 - 1, dtype=np.uint64)):
            with self.assertRaises(ValueError): execute(plan, {'input': bad})

    def test_plan_owns_constant_snapshot(self):
        plan = compile_graph(self.graph, self.approved)
        self.graph['nodes'][1]['value'][0][0] = 110
        result, _ = execute(plan, {'input': self.x})
        np.testing.assert_array_equal(result['square'], np.maximum(self.x @ self.w, 0) ** 2)

    def test_bad_shapes_and_forward_references_are_rejected(self):
        for shape in ([0, 32], [True, 32], [2, 2**40]):
            graph = copy.deepcopy(self.graph)
            graph['nodes'][0]['shape'] = shape
            with self.assertRaises(CompileError): compile_graph(graph)
        graph = copy.deepcopy(self.graph)
        graph['nodes'][2]['args'][0] = 'square'
        with self.assertRaises(CompileError): compile_graph(graph)

    def test_tiled_dispatch_preserves_exact_results_and_local_graph_tail(self):
        rng = np.random.default_rng(2901)
        for rows in (1, 63, 64, 65, 127, 128, 129, MAX_MASKED_ROWS):
            with self.subTest(rows=rows):
                graph = copy.deepcopy(self.graph)
                graph['nodes'][0]['shape'][0] = rows
                x = rng.integers(-10, 11, (rows, 32), dtype=np.int64)
                channel = RecordingChannel()
                result, routes = execute(compile_graph(graph, self.approved), {'input': x}, channel)
                np.testing.assert_array_equal(result['square'], np.maximum(x @ self.w, 0)**2)
                self.assertEqual(channel.rows, [min(rows, 64)])
                self.assertEqual([n for _, n in channel.calls], [min(64, rows-i) for i in range(0, rows, 64)])
                self.assertEqual(routes['activation'], 'local')
                self.assertEqual(routes['linear'], 'masked-field-gemm')
                self.assertTrue(channel.closed)

    def test_larger_graph_does_not_expand_constant_approval_or_worker_limits(self):
        for approved, rows in (([], 129), (self.approved, MAX_MASKED_ROWS+1)):
            graph = copy.deepcopy(self.graph)
            graph['nodes'][0]['shape'][0] = rows
            channel = RecordingChannel()
            execute(compile_graph(graph, approved), {'input': np.zeros((rows,32), dtype=np.int64)}, channel)
            self.assertFalse(channel.started)
            self.assertEqual(channel.calls, [])

    def test_invalid_last_row_refused_before_first_tile(self):
        graph = copy.deepcopy(self.graph)
        graph['nodes'][0]['shape'][0] = 129
        x = np.zeros((129,32), dtype=np.int64); x[-1,-1] = 11
        channel = RecordingChannel()
        with self.assertRaises(ValueError): execute(compile_graph(graph,self.approved), {'input':x},channel)
        self.assertEqual(channel.weights, [])
        self.assertEqual(channel.calls, [])
        self.assertTrue(channel.closed)

    def test_late_tile_failure_releases_no_graph_outputs_and_does_not_retry(self):
        class FailSecond(RecordingChannel):
            def matmul(self,node,x):
                if self.calls:
                    self.calls.append((node,x.shape[0]))
                    raise RuntimeError('late verified transport failure')
                return super().matmul(node,x)
        graph = copy.deepcopy(self.graph)
        graph['nodes'][0]['shape'][0] = 129
        channel=FailSecond()
        with self.assertRaisesRegex(RuntimeError,'late verified'):
            execute(compile_graph(graph,self.approved),{'input':np.ones((129,32),dtype=np.int64)},channel)
        self.assertEqual(channel.calls,[(0,64),(0,64)])
        self.assertTrue(channel.closed)


if __name__ == '__main__':
    unittest.main()
