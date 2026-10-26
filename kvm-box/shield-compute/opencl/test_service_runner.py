"""Host integration harness; deliberately not confidential-VM evidence."""
import json, os, subprocess, sys, threading, time
from pathlib import Path
sys.path.insert(0,os.environ.get('SHIELD_TEST_SERVICE_ROOT','/shield-code'))
import numpy as np
from compiler import constant_digest
from service import Compute,Server,Handler
mode=sys.argv[1] if len(sys.argv)>1 else 'masked'
weights=(np.arange(32*16,dtype=np.int64)%5-2).reshape(32,16)
audit=[]
warmup_audit=[]
attempts=0
class Audited(Compute):
 def graph(self,request):
  global attempts
  attempts+=1
  if mode.startswith('profiling'):time.sleep(.15)
  if mode=='busy' and attempts==1:raise ValueError('compute busy')
  if mode=='malformed':return {'ok':True,'outputs':{'c':[[1]]}}
  if mode=='overflow':return {'ok':True,'outputs':{'c':[[2**64-1]*16 for _ in range(4)]}}
  if mode in ('failure','profiling-failure','profiling-early-failure'):raise ValueError('injected service failure')
  if mode=='cuda-recovery' and attempts==1:raise ValueError('injected first-job rejection')
  value=super().graph(request);audit.append({'routes':value['routes'],'stats':value['stats']});return value
gpu_modes=('masked','busy','subbuffers','clone','out-of-order','svm','svm-subrange','svm-rebind','svm-free','cuda-repeat','cuda-recovery','profiling','profiling-out-of-order','profiling-early-release','profiling-late-release','profiling-callback-retain')
if mode not in gpu_modes + ('profiling-private','profiling-failure','profiling-early-failure','profiling-dependency-failure','binary','cuda-changed','private','failure','malformed','overflow','dependency-failure','svm-mixed'):
 raise ValueError('unknown test mode')
compute=Audited({'schema':'shield-guest-service/1','models':{},'publicConstants':[constant_digest(weights)] if mode in gpu_modes or mode in ('profiling','binary','cuda-changed') else [],'core':os.environ.get('SHIELD_TEST_CORE','/core/libshield-core.so'),'worker':{'host':'127.0.0.1','port':9741},'clientUids':[0]})
Path('/run/shield').mkdir(exist_ok=True)
path=Path('/run/shield/compute.sock');path.unlink(missing_ok=True)
with Server(str(path),Handler) as server:
 server.compute=compute
 thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
 try:
  app=os.environ.get('SHIELD_TEST_APP','/src/test-opencl')
  environment=dict(os.environ)
  if mode.startswith('profiling'):environment['SHIELD_TEST_MIN_DURATION_NS']='100000000'
  if app in ('/src/test-opencl','/src/test-svm'):environment['LD_LIBRARY_PATH']='/src/runtime'
  if mode=='cuda-repeat':
   subprocess.run([app],env=environment,timeout=60,check=True)
   assert len(audit)==1 and audit[0]['routes']['c']=='masked-field-gemm',audit
   assert audit[0]['stats']['exchanges']==1 and audit[0]['stats']['verification_failures']==0,audit
   warmup_audit.extend(audit)
   audit.clear();attempts=0
  result=subprocess.run([app]+(['failure'] if mode in ('failure','malformed','overflow') else [mode]),env=environment,timeout=60)
  if result.returncode:raise SystemExit(result.returncode)
  if mode in gpu_modes:
   assert len(audit)==1 and audit[0]['routes']['c']=='masked-field-gemm',audit
   assert audit[0]['stats']['exchanges']>0 and audit[0]['stats']['verification_failures']==0,audit
  elif mode in ('dependency-failure','profiling-dependency-failure'):assert attempts==0 and not audit
  elif mode in ('binary','svm-mixed','cuda-changed'):assert attempts==0 and not audit
  elif mode in ('private','profiling-private'):assert len(audit)==1 and audit[0]['routes']['c']=='local',audit
  elif mode in ('failure','malformed','overflow','profiling-failure','profiling-early-failure'):assert attempts==1 and not audit
  print(json.dumps({'environment':'host OpenCL integration; not enclave attestation','mode':mode,'attempts':attempts,'audit':audit,'warmupAudit':warmup_audit}),flush=True)
 finally:server.shutdown()
