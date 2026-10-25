import base64
import hashlib
import json
from pathlib import Path
import struct
import tempfile
import unittest
from unittest.mock import patch

from q8_service import ARITHMETIC, Q8Broker, Q8ExecutionError, validate_request


class Q8BrokerTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = Path(self.tmp.name)
        (root/'bin').mkdir()
        self.model_sha='a'*64
        self.tensor_sha='b'*64
        policy = root/'policy.json'
        policy.write_text(json.dumps({'schema':'shield-public-tensors/1',
            'modelSha256':self.model_sha,'tensors':[{'name':'public.weight','sha256':self.tensor_sha,
             'type':8,'ne':[32,8,1,1],'nbytes':272}]}))
        self.config={'sdk':str(root),'jobs':str(root),'timeoutSeconds':1,
            'models':{'llama':{'model':'/approved/model','policy':str(policy),
                'modelSha256':self.model_sha,'policySha256':hashlib.sha256(policy.read_bytes()).hexdigest()}}}
        helper=root/'bin/shield-q8-executor'
        helper.write_text('''#!/usr/bin/python3
import json,sys,struct,os,time
for line in sys.stdin.buffer:
 h=json.loads(line); data=sys.stdin.buffer.read(h['inputBytes'])
 mode=os.environ['MODE']
 if mode=='timeout': time.sleep(5)
 if mode=='decline':
  print(json.dumps({'ok':True,'supported':False,'outputBytes':0}),flush=True); continue
 stats={'offloaded':1,'local':0,'graph_macs':h['m']*h['k']*h['n'],'exchanges':1,
        'macs':h['m']*h['k']*h['n'],'verification_failures':0}
 reply={'ok':True,'supported':True,'outputBytes':h['m']*h['n']*4,
        'arithmetic':'shield-fixed-point-v1','stats':stats}
 if mode=='bad-stats': stats['verification_failures']=1
 if mode=='zero-macs': stats['macs']=0
 if mode=='local-exchange': stats['offloaded']=0; stats['local']=1
 if mode=='boolean-size': reply['outputBytes']=True
 if mode=='bad-contract': reply['arithmetic']='native-model-v1'
 if mode=='malformed': print('{',flush=True); sys.exit(0)
 print(json.dumps(reply),flush=True)
 if mode=='truncated': sys.stdout.buffer.write(b'\\0'); sys.stdout.buffer.flush(); sys.exit(0)
 output=struct.pack('<f',float('nan') if mode=='nan' else 1.5)*h['m']*h['n']
 sys.stdout.buffer.write(output); sys.stdout.buffer.flush()
''')
        helper.chmod(0o700)
        self.mode='success'
        self.broker=Q8Broker(self.config,lambda *a:{'MODE':self.mode})
        self.addCleanup(self.broker.close)
        self.request={'op':'q8_0_matmul','engine':'llama','tensor':'public.weight',
            'sha256':self.tensor_sha,'k':32,'n':8,'m':2,'arithmetic':ARITHMETIC,
            'input':base64.b64encode(struct.pack('<64f',*([0.25]*64))).decode()}

    def test_two_requests_reuse_process_and_return_verified_private_result(self):
        result=self.broker.execute(self.request)
        pid=self.broker.process.pid
        result2=self.broker.execute(self.request)
        self.assertEqual(pid,self.broker.process.pid)
        self.assertEqual(result,result2)
        self.assertEqual(struct.unpack('<16f',base64.b64decode(result['output'])),(1.5,)*16)
        self.assertEqual(result['policySha256'],self.config['models']['llama']['policySha256'])
        self.assertEqual(result['stats']['macs'],512)

    def test_decline_is_explicit_and_does_not_retire(self):
        self.mode='decline'
        self.assertEqual(self.broker.execute(self.request),{'ok':True,'supported':False})
        self.assertFalse(self.broker.retired)

    def test_bad_reference_rejected_before_process_creation(self):
        for key,value in [('sha256','c'*64),('tensor','private.weight'),('n',16)]:
            with self.subTest(key=key),patch('q8_service.subprocess.Popen') as spawn:
                with self.assertRaises(ValueError): self.broker.execute({**self.request,key:value})
                spawn.assert_not_called()
        self.assertFalse(self.broker.retired)

    def test_input_contract_rejects_invalid_encodings_shapes_and_privileged_fields(self):
        cases=[{'arithmetic':'native-model-v1'},{'k':True},{'m':33},{'n':0},
               {'input':'?'*len(self.request['input'])},{'input':self.request['input'][:-4]},
               {'input':base64.b64encode(struct.pack('<64f',*([float('nan')]*64))).decode()},
               {'worker':'attacker'},{'tensor':'public.weight\0'},{'engine':[]},
               {'m':1<<100},{'k':1<<100}]
        for change in cases:
            with self.subTest(change=list(change)):
                with self.assertRaises(ValueError):validate_request({**self.request,**change},self.config)
        self.assertIsNone(self.broker.process)

    def test_bad_replies_timeout_and_partial_output_retire_without_result(self):
        for mode in ('bad-stats','zero-macs','local-exchange','boolean-size','bad-contract','malformed','truncated','nan','timeout'):
            with self.subTest(mode=mode):
                broker=Q8Broker(self.config,lambda *a:{'MODE':mode})
                with self.assertRaises(Q8ExecutionError):broker.execute(self.request)
                self.assertTrue(broker.retired)
                self.assertIsNone(broker.process)
                with self.assertRaises(Q8ExecutionError):broker.execute(self.request)
                broker.close()

    def test_planned_turnover_replaces_executor_before_request_cap(self):
        with patch('q8_service.MAX_EXECUTOR_REQUESTS',2):
            self.broker.execute(self.request)
            first=self.broker.process
            self.broker.execute(self.request)
            self.assertIs(first,self.broker.process)
            self.broker.execute(self.request)
            self.assertIsNot(first,self.broker.process)
            self.assertIsNotNone(first.poll())
            self.assertFalse(self.broker.retired)
            self.assertEqual(self.broker.requests,1)

    def test_packed_weight_limit_refuses_before_process_launch(self):
        # Raw codes fit 512 MiB, but original 34-byte blocks do not.
        request={**self.request,'k':32768,'n':16000,'m':1,
            'input':base64.b64encode(bytes(32768*4)).decode()}
        with patch('q8_service.subprocess.Popen') as spawn:
            with self.assertRaisesRegex(ValueError,'shape limit'):self.broker.execute(request)
            spawn.assert_not_called()
        self.assertFalse(self.broker.retired)

    def test_changed_policy_is_not_loaded(self):
        Path(self.config['models']['llama']['policy']).write_text('{}')
        with self.assertRaisesRegex(ValueError,'digest'):Q8Broker(self.config,lambda *a:{})


if __name__=='__main__':unittest.main()
