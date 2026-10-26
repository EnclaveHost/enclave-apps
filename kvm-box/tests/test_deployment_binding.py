"""Cross-language wire identity and fail-closed startup regression checks."""
import base64
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest

ROOT=Path(__file__).resolve().parents[1]
spec=importlib.util.spec_from_file_location('binding_broker',ROOT/'host/broker.py')
broker=importlib.util.module_from_spec(spec);spec.loader.exec_module(broker)
ID='0x'+''.join(f'{i:02x}' for i in range(32))

class DeploymentBinding(unittest.TestCase):
 def test_exact_enclave_deployment_bytes(self):
  hd=broker.deployment_host_data(ID)
  option=broker.snp_guest_object(hd)
  encoded=option.split(',host-data=',1)[1]
  self.assertEqual(base64.b64decode(encoded,validate=True),bytes(range(32)))
  self.assertEqual(hd,ID[2:])
  self.assertEqual(broker.deployment_host_data('0x'+ID[2:].upper()),hd)

 def test_no_implicit_development_fallback(self):
  for bad in [None,'','0x'+'0'*64,'00'*32,ID+'0',ID[:-1],ID+'\n',' '+ID, '0X'+ID[2:], '0x'+'g'*64]:
   with self.subTest(bad=bad),self.assertRaises(ValueError):broker.deployment_host_data(bad)
  with self.assertRaises(ValueError):broker.deployment_host_data(ID,True)
  self.assertIsNone(broker.deployment_host_data(None,True))
  self.assertNotIn('host-data=',broker.snp_guest_object(None))

 def test_invalid_binding_rejected_before_work_created(self):
  with tempfile.TemporaryDirectory() as tmp:
   work=Path(tmp)/'must-not-exist'
   with self.assertRaises(ValueError):broker.Broker(SimpleNamespace(work=work))
   self.assertFalse(work.exists())

 def test_node_and_python_share_wire_vectors(self):
  ids=[ID,'0x'+ID[2:].upper(),'0x'+'ff'*32,'0x'+'0'*63+'1']
  bad=[None,'','0x'+'0'*64,ID[2:],ID+'0',ID[:-1],ID+'\n',' '+ID,'0X'+ID[2:],'0x'+'g'*64]
  args=[['--deployment-id',x] for x in ids]+[[] if x is None else ['--deployment-id',x] for x in bad]
  args += [['--deployment-id'],['--deployment-id',ID,'--development-unbound'],['--development-unbound','--development-unbound'],['--deployment-id',ID,'--deployment-id',ID],['--development-unbound']]
  script="import {deploymentBinding} from './host/deployment-binding.mjs'; let s='';for await(const c of process.stdin)s+=c;console.log(JSON.stringify(JSON.parse(s).map(a=>{try{return deploymentBinding(a)}catch{return null}})))"
  result=subprocess.run(['node','--input-type=module','-e',script],input=json.dumps(args),text=True,capture_output=True,cwd=ROOT,check=True)
  values=json.loads(result.stdout)
  for i,id in enumerate(ids):self.assertEqual(values[i]['hostData'],broker.deployment_host_data(id))
  self.assertEqual(values[len(ids):-1],[None]*(len(args)-len(ids)-1))
  self.assertEqual(values[-1],{'mode':'development-unbound'})

 def test_start_rejects_bad_binding_before_service_management(self):
  # Dummy readable paths are sufficient: no password is read and no systemd
  # action is allowed before malformed/missing binding arguments are refused.
  with tempfile.TemporaryDirectory() as tmp:
   for args in [[],['--deployment-id','0x'+'0'*64],['--development-unbound','--deployment-id',ID]]:
    p=subprocess.run(['bash',str(ROOT/'host/start.sh'),*[tmp]*6,*args],text=True,capture_output=True)
    self.assertEqual(p.returncode,2,p.stderr)
    self.assertNotIn('Build first',p.stderr)

 def test_broker_cli_rejects_abbreviations_and_duplicate_forms(self):
  with tempfile.TemporaryDirectory() as tmp:
   work=Path(tmp)/'not-created'
   common=['--release',tmp,'--work',str(work),'--enclave',tmp,'--vcek',tmp,'--chain',tmp,'--min-tcb',tmp]
   cases=[['--dev'],['--dep',ID],['--deployment-id',ID,'--deployment-id='+ID],['--deployment-id='+ID,'--deployment-id='+ID],['--deployment-id='+ID,'--development-unbound']]
   for args in cases:
    with self.subTest(args=args):
     p=subprocess.run(['python3',str(ROOT/'host/broker.py'),*common,*args],text=True,capture_output=True)
     self.assertEqual(p.returncode,2,p.stderr);self.assertFalse(work.exists())

 def test_symlinked_start_still_validates_before_services(self):
  with tempfile.TemporaryDirectory() as tmp:
   link=Path(tmp)/'linked-checkout';link.symlink_to(ROOT,target_is_directory=True)
   for options in ['', '--preserve-symlinks-main']:
    p=subprocess.run(['bash',str(link/'host/start.sh'),*[tmp]*6,'--deployment-id','0x'+'0'*64],text=True,capture_output=True,env={**os.environ,'NODE_OPTIONS':options})
    self.assertEqual(p.returncode,2,p.stderr);self.assertNotIn('Build first',p.stderr)

 def test_node_equals_forms_do_not_hide_a_second_binding(self):
  script="import {deploymentBinding} from './host/deployment-binding.mjs';const id="+json.dumps(ID)+";console.log(JSON.stringify([['--deployment-id='+id],['--deployment-id='+id,'--development-unbound'],['--deployment-id',id,'--deployment-id='+id],['--development-unbound=true']].map(a=>{try{return deploymentBinding(a)}catch{return null}})))"
  p=subprocess.run(['node','--input-type=module','-e',script],text=True,capture_output=True,cwd=ROOT,check=True)
  values=json.loads(p.stdout);self.assertEqual(values[0]['hostData'],ID[2:]);self.assertEqual(values[1:],[None,None,None])

if __name__=='__main__':unittest.main()
