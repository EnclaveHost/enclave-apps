import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

APP=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('config_builder',APP/'guest/native/build_service_config.py')
builder=importlib.util.module_from_spec(spec);spec.loader.exec_module(builder)


class Config(unittest.TestCase):
    def setUp(self):
        self.temp=tempfile.TemporaryDirectory();self.root=Path(self.temp.name)
        self.policies=self.root/'policies';self.policies.mkdir()
        self.source=self.root/'service.json';self.output=self.root/'output'
        self.config={'schema':'shield-guest-service/1','sdk':'/opt/shield','core':'/opt/shield/lib/libshield-core.so',
                     'socket':'/run/shield/compute.sock','jobs':'/run/shield/jobs','models':{},
                     'worker':{'host':'10.0.2.2','port':9741},'clientUids':[0,1000],
                     'threads':2,'timeoutSeconds':1200,'publicConstants':['b'*64],'enableQ8Experimental':True}
        for engine,(stem,filename) in builder.MODELS.items():
            calibration=b'synthetic calibration '+engine.encode()
            policy=json.dumps({'modelSha256':'a'*64,'calibrationSha256':hashlib.sha256(calibration).hexdigest()}).encode()
            (self.policies/(stem+'.calib')).write_bytes(calibration)
            (self.policies/(stem+'.json')).write_bytes(policy)
            self.config['models'][engine]={'model':'/models/'+filename,'modelSha256':'a'*64,
                'policy':'/shield/policy/'+stem+'.json','policySha256':hashlib.sha256(policy).hexdigest(),
                'calibration':'/shield/policy/'+stem+'.calib','reserveBytes':1000000}
        self.save()

    def tearDown(self): self.temp.cleanup()
    def save(self): self.source.write_text(json.dumps(self.config))
    def build(self): return builder.build(self.source,self.policies,self.output)

    def test_fixed_route_without_expanding_model_approvals(self):
        receipt=self.build();result=json.loads((self.output/'native-service.json').read_text())
        expected=copy.deepcopy(self.config)
        expected['worker']={'host':'unix:/run/shield-worker/gpu0','port':19595}
        expected['clientUids']=[1000,1003]
        self.assertEqual(result,expected)
        self.assertFalse(receipt['runtimeAdmission']);self.assertFalse(receipt['modelApprovalsChanged'])
        self.assertEqual(receipt['configSha256'],hashlib.sha256((self.output/'native-service.json').read_bytes()).hexdigest())

    def test_changed_input_route_refused(self):
        self.config['worker']['host']='example.invalid';self.save()
        with self.assertRaises(ValueError): self.build()
        self.assertFalse(self.output.exists())

    def test_extra_config_or_wrong_numeric_type_refused(self):
        self.config['extra']='value';self.save()
        with self.assertRaises(ValueError): self.build()
        del self.config['extra'];self.config['threads']=True;self.save()
        with self.assertRaises(ValueError): self.build()

    def test_duplicate_json_field_refused(self):
        self.source.write_text(self.source.read_text().replace('"threads": 2','"threads": 2, "threads": 3'))
        with self.assertRaises(ValueError): self.build()

    def test_calibration_or_policy_change_refused(self):
        p=self.policies/'qwen.calib';old=p.read_bytes();p.write_bytes(b'changed')
        with self.assertRaises(ValueError): self.build()
        p.write_bytes(old);(self.policies/'qwen.json').write_text('{}')
        with self.assertRaises(ValueError): self.build()

    def test_path_or_model_identity_change_refused(self):
        self.config['models']['llama']['model']='/tmp/arbitrary.gguf';self.save()
        with self.assertRaises(ValueError): self.build()
        self.config['models']['llama']['model']='/models/qwen2.5-0.5b-instruct-q8_0.gguf'
        self.config['models']['llama']['modelSha256']='c'*64;self.save()
        with self.assertRaises(ValueError): self.build()

    def test_symlinked_calibration_refused(self):
        p=self.policies/'qwen.calib';external=self.root/'external'
        p.rename(external);p.symlink_to(external)
        with self.assertRaises(OSError): self.build()
        self.assertFalse(self.output.exists())

    def test_existing_output_preserved(self):
        self.output.mkdir();(self.output/'keep').write_text('preserve')
        with self.assertRaises(FileExistsError): self.build()
        self.assertEqual((self.output/'keep').read_text(),'preserve')


if __name__=='__main__':unittest.main(verbosity=2)
