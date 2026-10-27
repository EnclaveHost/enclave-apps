import hashlib
import json
import os
from pathlib import Path
import socket
import struct
import tempfile
import threading
import unittest
from unittest.mock import patch

from service import Compute, receive, send, FIXED_ARITHMETIC, NATIVE_ARITHMETIC


class ServiceTests(unittest.TestCase):
    def test_graph_failure_exposes_only_bounded_code_and_remains_retired(self):
        import numpy as np
        from channel import ChannelError
        from compiler import constant_digest
        w = np.ones((32, 4), dtype=np.int64)
        self.config['publicConstants'] = [constant_digest(w)]
        self.config['core'] = '/unused/test-core.so'
        request = {'op': 'graph', 'mode': 'masked', 'inputs': {'x': [[1]*32]*65},
                   'graph': {'schema': 'shield-graph/1', 'nodes': [
                       {'name': 'x', 'op': 'input', 'shape': [65,32], 'bound': 1},
                       {'name': 'w', 'op': 'constant', 'shape': [32,4], 'value': w.tolist()},
                       {'name': 'y', 'op': 'matmul', 'args': ['x','w']},
                   ], 'outputs': ['y']}}
        before = dict(self.compute.telemetry)
        with patch('channel.MaskedChannel') as factory:
            channel = factory.return_value
            channel.matmul.side_effect = [np.full((64,4),32,dtype=np.int64),
                ChannelError('private tensor or arbitrary worker text', stage='compute', code=-10)]
            with self.assertRaisesRegex(ValueError, '^masked computation failed; no result released$'):
                self.compute.run(request)
            channel.close.assert_called_once()
            self.assertEqual(channel.matmul.call_count, 2)
            with self.assertRaisesRegex(ValueError, 'retired'):
                self.compute.run(request)
            factory.assert_called_once()
        status = self.compute.status()
        self.assertEqual(status['state'], 'retired')
        self.assertEqual(status['graphFailure'], {'stage': 'compute', 'code': -10})
        self.assertEqual(status['telemetry'], before)
        self.assertEqual(status['completed'], 0)
        self.assertNotIn('private tensor', json.dumps(status))
        status['graphFailure']['stage'] = 'changed by caller'
        self.assertEqual(self.compute.status()['graphFailure']['stage'], 'compute')

    def test_channel_diagnostic_drops_unbounded_or_arbitrary_metadata(self):
        from channel import ChannelError
        for code in ('secret', True, 0, -129, 1 << 60, None):
            error = ChannelError('private', stage='private worker string', code=code)
            self.assertEqual(error.public_failure(), {'stage': 'unknown', 'code': None})

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        model, calib, policy = [self.root / x for x in ('model', 'calib', 'policy')]
        model.write_bytes(b'public model fixture')
        calib.write_bytes(b'public calibration fixture')
        sha = lambda p: hashlib.sha256(p.read_bytes()).hexdigest()
        policy.write_text(json.dumps({'modelSha256': sha(model), 'calibrationSha256': sha(calib)}))
        spec = dict(model=str(model), modelSha256=sha(model), policy=str(policy),
                    policySha256=sha(policy), calibration=str(calib), reserveBytes=128 << 20)
        self.config = dict(schema='shield-guest-service/1', sdk='/opt/shield',
                           models={'llama': spec, 'sd': spec}, jobs=str(self.root),
                           worker={'host': '10.0.2.2', 'port': 9741}, threads=2,
                           timeoutSeconds=10, publicConstants=[])
        self.compute = Compute(self.config)

    def test_modified_model_prevents_service_start(self):
        (self.root / 'model').write_bytes(b'host replacement')
        with self.assertRaisesRegex(ValueError, 'digest mismatch'):
            Compute(self.config)

    def test_modified_calibration_prevents_service_start(self):
        (self.root / 'calib').write_bytes(b'host replacement')
        with self.assertRaisesRegex(ValueError, 'digest mismatch'):
            Compute(self.config)

    def test_modified_policy_prevents_service_start(self):
        (self.root / 'policy').write_text('{}')
        with self.assertRaisesRegex(ValueError, 'digest mismatch'):
            Compute(self.config)

    def test_environment_is_allowlisted(self):
        with patch.dict(os.environ, {'SHIELDED_FAULT_DIAG_PLAINTEXT': '1',
                'SHIELDED_PAD_SOURCE': '/attacker', 'SHIELDED_HOST': 'attacker',
                'SHIELDED_WIDE': '0',
                'LD_LIBRARY_PATH': '/attacker', 'GGML_BACKEND_PATH': '/cuda.so',
                'PYTHONPATH': '/attacker', 'LD_PRELOAD': '/attacker.so'}):
            env = self.compute.environment(self.config['models']['llama'], True, '/private', 'llama')
            sd_env = self.compute.environment(self.config['models']['sd'], True, '/private', 'sd')
        self.assertEqual(env['SHIELDED_HOST'], '10.0.2.2')
        self.assertEqual(env['GGML_BACKEND_PATH'], '/opt/shield/lib/backends/libggml-cpu.so')
        self.assertNotIn('SHIELDED_WIDE', env)
        self.assertEqual(sd_env['SHIELDED_WIDE'], '1')
        for key in ('SHIELDED_FAULT_DIAG_PLAINTEXT', 'SHIELDED_PAD_SOURCE', 'LD_LIBRARY_PATH', 'PYTHONPATH'):
            self.assertNotIn(key, env)

    def test_cpu_environment_never_loads_shield_or_gpu(self):
        env = self.compute.environment(self.config['models']['llama'], False, '/private', 'llama')
        self.assertFalse(any(k.startswith(('SHIELDED_', 'ENCLAVE_SHIELD_', 'SD_ACCEL')) for k in env))
        self.assertNotIn('LD_PRELOAD', env)

    def test_native_arithmetic_contract_is_required_before_launch(self):
        before = self.compute.status()
        for arithmetic in (None, True, 'float32', 'native-model-v1', {}, 'shield-fixed-point-v2'):
            request = {'op': 'llama', 'prompt': 'test', 'mode': 'masked'}
            if arithmetic is not None:
                request['arithmetic'] = arithmetic
            with self.subTest(arithmetic=arithmetic), patch('service.subprocess.Popen') as run:
                with self.assertRaisesRegex(ValueError, 'requires arithmetic'):
                    self.compute.run(request)
                run.assert_not_called()
            self.assertEqual(before, self.compute.status())
        with patch('service.subprocess.Popen') as run:
            with self.assertRaisesRegex(ValueError, 'CPU mode requires'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'mode': 'cpu',
                                  'arithmetic': FIXED_ARITHMETIC})
            run.assert_not_called()

    def test_native_result_identifies_exact_authenticated_arithmetic_profile(self):
        def complete(command, **kw):
            kw['stdout'].write(b'result')
            if kw['env'].get('LD_PRELOAD'):
                self.assertEqual(kw['env']['ENCLAVE_SHIELD_ARITHMETIC'], FIXED_ARITHMETIC)
                kw['stderr'].write(b'[enclave-shield] offloaded=1 local=0 graph_macs=32 exchanges=1 macs=32 verification_failures=0\n')
            from unittest.mock import Mock
            return Mock(returncode=0)
        with patch('service.subprocess.Popen', side_effect=complete):
            masked = self.compute.run({'op': 'llama', 'prompt': 'test',
                                       'arithmetic': FIXED_ARITHMETIC})
            cpu = self.compute.run({'op': 'llama', 'prompt': 'test', 'mode': 'cpu'})
        spec = self.config['models']['llama']
        self.assertEqual(masked['arithmetic'], {'id': FIXED_ARITHMETIC, 'approximate': True,
            'modelSha256': spec['modelSha256'], 'policySha256': spec['policySha256'],
            'calibrationSha256': hashlib.sha256(Path(spec['calibration']).read_bytes()).hexdigest()})
        self.assertEqual(cpu['arithmetic'], {'id': NATIVE_ARITHMETIC, 'approximate': False,
            'modelSha256': spec['modelSha256']})

    def test_client_cannot_set_policy_backend_or_worker(self):
        for key in ('env', 'worker', 'backend', 'model', 'policy', 'command', 'args', 'publicConstants'):
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, 'unsupported request fields'):
                self.compute.run({'op': 'llama', 'prompt': 'test', key: 'attacker'})

    def test_unknown_gpu_operation_rejected(self):
        for op in ('cuda', 'upload', 'launch', 'rpc', 'shell'):
            with self.subTest(op=op), self.assertRaisesRegex(ValueError, 'unsupported engine'):
                self.compute.run({'op': op, 'prompt': 'test'})

    def test_bad_limits_are_rejected_before_launch(self):
        for fields in ({'seed': -1}, {'tokens': 257}, {'tokens': True}, {'prompt': '\0'},
                       {'prompt': 'x' * 8193}, {'mode': 'cuda'}):
            with self.subTest(fields=list(fields)), patch('service.subprocess.Popen') as run:
                with self.assertRaises(ValueError):
                    self.compute.run({'op': 'llama', 'prompt': 'test', **fields})
                run.assert_not_called()

    def test_process_failure_retires_without_results(self):
        with patch('service.subprocess.Popen') as run:
            run.return_value.returncode = 1
            with self.assertRaisesRegex(ValueError, 'no result released'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
            self.assertTrue(self.compute.retired)
            with self.assertRaisesRegex(ValueError, 'retired'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
            self.assertEqual(run.call_count, 1)

    def test_missing_verification_stats_refuses_success(self):
        with patch('service.subprocess.Popen') as run:
            run.return_value.returncode = 0
            with self.assertRaisesRegex(ValueError, 'no result released'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
        self.assertTrue(self.compute.retired)

    def test_concurrent_work_is_not_started(self):
        self.compute.lock.acquire()
        try:
            with self.assertRaisesRegex(ValueError, 'busy'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
        finally:
            self.compute.lock.release()

    def test_private_constant_remains_cpu(self):
        graph = {'schema': 'shield-graph/1', 'nodes': [
            {'name': 'x', 'op': 'input', 'shape': [1, 32], 'bound': 2},
            {'name': 'w', 'op': 'constant', 'shape': [32, 1], 'value': [[1]] * 32},
            {'name': 'y', 'op': 'matmul', 'args': ['x', 'w']}], 'outputs': ['y']}
        with patch('channel.MaskedChannel') as channel:
            result = self.compute.run({'op': 'graph', 'graph': graph,
                                       'inputs': {'x': [[2] * 32]}, 'mode': 'masked'})
            channel.assert_not_called()
        self.assertEqual(result['outputs']['y'], [[64]])
        self.assertEqual(result['routes'], {'y': 'local'})
        status = self.compute.status()
        self.assertEqual(status['telemetry']['verifiedOffloads'], 0)
        self.assertEqual(status['telemetry']['maskedMacs'], 0)
        self.assertEqual(status['telemetry']['lastCompleted']['engine'], 'graph')
        self.assertEqual(status['completed'], 1)
        self.assertIsNone(status['active'])

    def test_telemetry_records_success_without_private_content(self):
        def complete_process(command, **kw):
            kw['stdout'].write(b'private generated text')
            kw['stderr'].write(b'[enclave-shield] offloaded=7 local=3 graph_macs=999 exchanges=11 macs=224 verification_failures=0\n')
            from unittest.mock import Mock
            return Mock(returncode=0)
        with patch('service.subprocess.Popen', side_effect=complete_process):
            self.compute.run({'op': 'llama', 'prompt': 'private prompt', 'arithmetic': FIXED_ARITHMETIC})
        status = self.compute.status()
        self.assertEqual(status['telemetry']['verifiedOffloads'], 11)
        self.assertEqual(status['telemetry']['maskedMacs'], 224)
        self.assertNotIn('private', json.dumps(status))
        previous = status['telemetry']
        with patch('service.subprocess.Popen') as run:
            run.return_value.returncode = 1
            with self.assertRaises(ValueError):
                self.compute.run({'op': 'llama', 'prompt': 'private prompt', 'arithmetic': FIXED_ARITHMETIC})
        self.assertEqual(self.compute.status()['telemetry'], previous)

    def test_legacy_native_counts_are_not_accepted_as_verified_work(self):
        def legacy_process(command, **kw):
            kw['stdout'].write(b'private generated text')
            kw['stderr'].write(b'[enclave-shield] offloaded=7 local=3 macs=999 verification_failures=0\n')
            from unittest.mock import Mock
            return Mock(returncode=0)
        with patch('service.subprocess.Popen', side_effect=legacy_process):
            with self.assertRaisesRegex(ValueError, 'no result released'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
        self.assertTrue(self.compute.retired)
        self.assertEqual(self.compute.completed, 0)
        self.assertEqual(self.compute.status()['telemetry']['maskedMacs'], 0)

    def test_local_native_products_do_not_increment_gpu_counters(self):
        def local_process(command, **kw):
            kw['stdout'].write(b'local result')
            kw['stderr'].write(b'[enclave-shield] offloaded=0 local=3 graph_macs=999 exchanges=0 macs=0 verification_failures=0\n')
            from unittest.mock import Mock
            return Mock(returncode=0)
        with patch('service.subprocess.Popen', side_effect=local_process):
            result = self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
        self.assertEqual(result['stats']['graph_macs'], 999)
        self.assertEqual(self.compute.status()['telemetry']['verifiedOffloads'], 0)
        self.assertEqual(self.compute.status()['telemetry']['maskedMacs'], 0)

    def test_failed_native_job_does_not_publish_earlier_verified_chunks(self):
        def failed_process(command, **kw):
            kw['stdout'].write(b'partial private result')
            kw['stderr'].write(b'[enclave-shield] offloaded=0 local=0 graph_macs=0 exchanges=2 macs=128 verification_failures=1\n')
            from unittest.mock import Mock
            return Mock(returncode=1)
        with patch('service.subprocess.Popen', side_effect=failed_process):
            with self.assertRaisesRegex(ValueError, 'no result released'):
                self.compute.run({'op': 'llama', 'prompt': 'test', 'arithmetic': FIXED_ARITHMETIC})
        self.assertTrue(self.compute.retired)
        self.assertEqual(self.compute.completed, 0)
        self.assertEqual(self.compute.status()['telemetry']['verifiedOffloads'], 0)
        self.assertEqual(self.compute.status()['telemetry']['maskedMacs'], 0)

    def test_framing_size_and_truncation(self):
        for data in (struct.pack('!I', 999), struct.pack('!I', 0), struct.pack('!I', 5) + b'{}'):
            a, b = socket.socketpair()
            with a, b:
                a.sendall(data); a.shutdown(socket.SHUT_WR)
                with self.assertRaises(ValueError): receive(b, 100)

    def test_framing_roundtrip(self):
        a, b = socket.socketpair()
        with a, b:
            send(a, {'text': 'private guest data'})
            self.assertEqual(receive(b, 100), {'text': 'private guest data'})


if __name__ == '__main__':
    unittest.main()
