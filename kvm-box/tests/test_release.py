import importlib.util,json,tempfile,unittest
from pathlib import Path
from types import SimpleNamespace
spec=importlib.util.spec_from_file_location('broker',Path(__file__).resolve().parents[1]/'host/broker.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
class ReleaseTests(unittest.TestCase):
 def setUp(self):
  self.tmp=tempfile.TemporaryDirectory();self.addCleanup(self.tmp.cleanup);self.root=Path(self.tmp.name)
  for n in ['kernel','firmware','controller','desktop']:(self.root/n).write_bytes(n.encode())
  item=lambda n:{'file':n,'sha256':module.sha(self.root/n)}
  self.manifest={'kernel':item('kernel'),'firmware':item('firmware'),'images':{n:{'image':n,'sha256':module.sha(self.root/n)}for n in ['controller','desktop']}}
  (self.root/'release.json').write_text(json.dumps(self.manifest))
  a=SimpleNamespace(release=self.root,work=self.root/'work',enclave=self.root,parent_unit='',vcek=self.root,chain=self.root,min_tcb=self.root,development_unbound=True)
  self.b=module.Broker(a)
 def test_valid_inputs(self):self.b.check_files('desktop')
 def test_changed_image_is_refused(self):
  (self.root/'desktop').write_bytes(b'substituted')
  with self.assertRaisesRegex(ValueError,'digest mismatch'):self.b.check_files('desktop')
 def test_changed_firmware_is_refused(self):
  (self.root/'firmware').write_bytes(b'substituted')
  with self.assertRaisesRegex(ValueError,'digest mismatch'):self.b.check_files('desktop')
 def test_path_escape_is_refused(self):
  self.b.release['images']['desktop']['image']='../desktop'
  with self.assertRaisesRegex(ValueError,'digest mismatch'):self.b.check_files('desktop')
 def test_other_vsock_caller_refused(self):
  dummy=SimpleNamespace(broker=self.b)
  self.assertTrue(module.Server.verify_request(dummy,None,(self.b.cids['controller'],1234)))
  self.assertFalse(module.Server.verify_request(dummy,None,(self.b.cids['desktop'],1234)))
  self.assertFalse(module.Server.verify_request(dummy,None,(2,1234)))
 def test_changed_model_volume_is_refused(self):
  (self.root/'models.img').write_bytes(b'approved public models')
  self.b.release['images']['desktop']['volumes']=[{'file':'models.img','sha256':module.sha(self.root/'models.img')}]
  self.b.check_files('desktop')
  (self.root/'models.img').write_bytes(b'substituted')
  with self.assertRaisesRegex(ValueError,'digest mismatch'):self.b.check_files('desktop')
 def test_model_path_escape_is_refused(self):
  self.b.release['images']['desktop']['volumes']=[{'file':'../models.img','sha256':'0'*64}]
  with self.assertRaisesRegex(ValueError,'digest mismatch'):self.b.check_files('desktop')
if __name__=='__main__':unittest.main()
