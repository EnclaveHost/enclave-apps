import copy,unittest
from accept import accept
class Acceptance(unittest.TestCase):
 def setUp(self):
  identity='01234567-0123-4567-89ab-0123456789abc';image=4096;disk=len(b'synthetic-native-checkpoint-disk-state-v1');cipher=40+176+image+disk+40
  first='\n'.join(['ROUNDTRIP BEGIN encrypted-fresh-boot-v1 PUBLIC_TEST_KEY','ROUNDTRIP TPM_BEFORE OK','ROUNDTRIP CREATE begin','ROUNDTRIP CREATE OK original=1',f'ROUNDTRIP IMAGE_SIZE {image}',f'ROUNDTRIP ENCRYPTED_SAVED bytes={cipher}','ROUNDTRIP POST_CAPTURE_RAM_CHANGED','ROUNDTRIP CAPTURE_POWER_OFF'])
  second='\n'.join(['ROUNDTRIP BEGIN encrypted-fresh-boot-v1 PUBLIC_TEST_KEY',f'ROUNDTRIP AUTHENTICATED memory={image} disk={disk}','ROUNDTRIP ATOMIC_RESTORE begin','ROUNDTRIP CREATE OK original=0','ROUNDTRIP RESUMED_CLOSE OK','ROUNDTRIP RAM_CONTINUATION OK bytes=1048576','ROUNDTRIP TPM_AFTER OK','ROUNDTRIP RESULT PASS synthetic-resume-only'])
  self.report={'result.json':{'id':identity,'cleanup':{'id':identity,'absent':True},'masterSha':'a'*64,'captureVhdSha':'b'*64,'captureVhdBytes':1024**2,'phases':[{'phase':'capture','capture':first,'off':True,'marker':True,'startedAt':'01','finishedAt':'02'},{'phase':'restore','capture':second,'off':True,'marker':True,'startedAt':'03','finishedAt':'04'}]},'watchdog-receipt.json':{'absent':True,'secondAbsenceCheck':{'absent':True}}}
  self.before={'vms':[{'Id':'existing','State':'Running'}],'inputs':[{'path':'C:\\type1.vmgs','sha256':'a'*64}]};self.after=copy.deepcopy(self.before);self.cleanup={'vmAbsent':True,'tasksAbsent':True}
 def check(self):return accept(self.report,self.before,self.after,self.cleanup)
 def test_synthetic_is_not_app_qualification(self):
  v=self.check();self.assertTrue(v['syntheticEncryptedColdBootContinuation']);self.assertFalse(v['privateSnapshotAppQualified'])
 def test_every_required_line(self):
  for phase in self.report['result.json']['phases']:
   original=phase['capture']
   for line in original.splitlines():
    for bad in [original.replace(line,''),original+'\n'+line]:
     phase['capture']=bad
     with self.assertRaises(ValueError):self.check()
   phase['capture']=original
 def test_no_poweroff(self):
  self.report['result.json']['phases'][0]['off']=False
  with self.assertRaises(ValueError):self.check()
 def test_failure_even_with_pass(self):
  self.report['result.json']['phases'][1]['capture']+='\nROUNDTRIP RESULT FAIL broken'
  with self.assertRaises(ValueError):self.check()
 def test_overlap(self):
  self.report['result.json']['phases'][1]['startedAt']='01'
  with self.assertRaises(ValueError):self.check()
 def test_inventory_changed(self):
  self.after['vms'][0]['State']='Off'
  with self.assertRaises(ValueError):self.check()
 def test_tasks_remaining(self):
  self.cleanup['tasksAbsent']=False
  with self.assertRaises(ValueError):self.check()
 def test_wrong_encrypted_size(self):
  p=self.report['result.json']['phases'][0];p['capture']=p['capture'].replace('ENCRYPTED_SAVED bytes=','ENCRYPTED_SAVED bytes=1')
  with self.assertRaises(ValueError):self.check()
if __name__=='__main__':unittest.main()
