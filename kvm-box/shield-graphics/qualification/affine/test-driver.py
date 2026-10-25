"""Container-only end-to-end test of the patched ICD, not a native VM proof."""
import json,os,select,socket,struct,subprocess,sys,threading,time
from pathlib import Path
import numpy as np
sys.path.insert(0,'/compute')
from service import Compute,receive,send
from compiler import constant_digest
assert os.getuid()==0 and Path('/output/build/prepass').is_file()
assert not Path('/dev/dri').exists() and not Path('/dev/nvidia0').exists()
mode=sys.argv[1];gl_mode=mode.startswith('gl-');failure=mode.startswith('bad-reply') or mode in ['wrong-peer','worker-corrupt','gl-worker-corrupt'];variant=mode in ['queries','generated','bad-reply-timeline','bad-reply-timeline-before','bad-reply-queue-idle','bad-reply-device-idle'];assert gl_mode or variant or mode in ['equivalent','masked','local','bad-reply','wrong-peer','unavailable','worker-corrupt','unmatched','dispatch-base','specialization','dynamic-bindings','wide-range']
no_route=mode in ['gl-queries','gl-wide-range','gl-descriptor-buffer','queries','generated','unmatched','dispatch-base','specialization','dynamic-bindings','wide-range']
root=Path('/run/shield');root.mkdir(mode=0o755);os.chown(root,1000 if mode=='wrong-peer' else 1001,1000)
worker_dir=Path('/run/shield-worker');worker_dir.mkdir(mode=0o755)
broker=socket.socket(socket.AF_UNIX);broker.bind('/run/shield-worker/gpu0');os.chown('/run/shield-worker/gpu0',0,1001);os.chmod('/run/shield-worker/gpu0',0o660);broker.listen(8);broker.settimeout(2)
def drop(uid):
 def apply():os.setgroups([1001] if uid==1001 else []);os.setgid(1000);os.setuid(uid)
 return apply
ready_r,ready_w=os.pipe();report_r,report_w=os.pipe();pid=os.fork()
if pid==0:
 os.close(ready_r);os.close(report_r);broker.close();drop(1000 if mode=='wrong-peer' else 1001)()
 record={'passed':False,'requests':0}
 try:
  if mode not in ['unavailable','gl-unavailable']:
   server=socket.socket(socket.AF_UNIX);server.bind('/run/shield/compute.sock');os.chmod('/run/shield/compute.sock',0o660);server.listen(8);server.settimeout(8)
  os.write(ready_w,b'R');os.close(ready_w)
  weights=np.zeros((32,4),dtype=np.int64);weights[:4]=[[1,1,1,-1],[-1,1,0,2],[0,0,2,-1],[0,0,1,1]];digest=constant_digest(weights)
  compute=Compute({'schema':'shield-guest-service/1','models':{},'publicConstants':[digest] if mode in ['masked','worker-corrupt','equivalent','gl-masked','gl-worker-corrupt'] else [],'core':'/opt/shield/lib/libshield-core.so','worker':{'host':'unix:/run/shield-worker/gpu0','port':19595}})
  expected=8 if mode in ['local','masked','equivalent','gl-masked','gl-local'] else 0 if mode in ['unavailable','gl-unavailable'] or no_route else 1
  for index in range(expected):
   conn,_=server.accept();conn.settimeout(20)
   with conn:
    if mode=='wrong-peer':assert conn.recv(1)==b'';continue
    request=receive(conn,16384);record['requests']+=1
    assert request['graph']['nodes'][1]['shape']==[32,4] and request['graph']['nodes'][1]['value']==weights.tolist()
    assert len(request['graph']['nodes'])==3 and request['graph']['nodes'][2]=={'name':'y','op':'matmul','args':['x','w']}
    record['matrixDigest']=digest;record['fixtureMatrixMatched']=True
    if mode.startswith('bad-reply'):
     time.sleep(.2)
     send(conn,{'ok':True,'outputs':{'y':[[0]*4]*63+[[0,0,0,1.5]]},'routes':{'y':'local'},'stats':{}})
    else:
     try:result=compute.run(request)
     except ValueError:
      assert mode in ['worker-corrupt','gl-worker-corrupt'] and compute.retired
      result={'ok':False,'error':'protected computation rejected'}
      record['serviceRetired']=True
      try:compute.run(request)
      except ValueError as error:assert 'retired' in str(error)
      else:raise AssertionError('retired service accepted retry')
     send(conn,result)
     if mode not in ['worker-corrupt','gl-worker-corrupt']:assert result['routes']=={'y':'masked-field-gemm' if mode in ['masked','equivalent','gl-masked','gl-worker-corrupt'] else 'local'}
  if no_route:
   server.settimeout(2)
   try:unexpected,_=server.accept()
   except socket.timeout:pass
   else:unexpected.close();raise AssertionError('unmatched shader contacted service')
  record.update(passed=True,telemetry=compute.telemetry)
 except Exception as error:record.update(error=type(error).__name__,detail=str(error)[:200])
 os.write(report_w,json.dumps(record).encode());os.close(report_w);os._exit(0 if record['passed'] else 1)
os.close(ready_w);os.close(report_w);assert os.read(ready_r,1)==b'R';os.close(ready_r)
proxy_report={'connections':0,'bytesToWorker':0,'bytesFromWorker':0};done=threading.Event()
def proxy():
 try:
  while not done.is_set():
   try:peer,_=broker.accept()
   except socket.timeout:continue
   _,uid,_=struct.unpack('3i',peer.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12));assert uid==1001
   target=socket.create_connection(('127.0.0.1',int(os.environ['SHIELD_TEST_WORKER_PORT'])),5);proxy_report['connections']+=1
   with peer,target:
    if mode in ['worker-corrupt','gl-worker-corrupt']:
     def exact(sock,n):
      value=bytearray()
      while len(value)<n:
       part=sock.recv(n-len(value))
       if not part:raise EOFError
       value.extend(part)
      return bytes(value)
     peer.settimeout(15);target.settimeout(15)
     try:
      while True:
       header=exact(peer,9);command,size=struct.unpack('<BQ',header);assert size<=16*1024*1024
       payload=exact(peer,size);target.sendall(header+payload);proxy_report['bytesToWorker']+=9+size
       reply_header=exact(target,9);status,size=struct.unpack('<BQ',reply_header);assert size<=16*1024*1024
       reply=bytearray(exact(target,size))
       if command in (12,13) and status==0 and reply:
        reply[0]^=1;proxy_report['modifiedRealGemmReply']=True
       peer.sendall(reply_header+reply);proxy_report['bytesFromWorker']+=9+size
     except EOFError:pass
     continue
    ended=False
    while not ended:
     readable,_,_=select.select([peer,target],[],[],20)
     if not readable:raise TimeoutError('proxy deadline')
     for source in readable:
      data=source.recv(65536)
      if not data:ended=True;break
      destination=target if source is peer else peer
      proxy_report['bytesToWorker' if source is peer else 'bytesFromWorker']+=len(data);destination.sendall(data)
 except Exception as error:proxy_report.update(error=type(error).__name__)
 finally:broker.close()
thread=threading.Thread(target=proxy);thread.start()
command=['/output/variants/prepass-'+mode if variant or no_route and mode!='unmatched' else '/output/build/prepass','/output/build-equivalent' if mode=='equivalent' else '/output/build-unmatched' if mode=='unmatched' else '/output/build','/output/driver-'+mode+'.rgba']
if gl_mode:command=['xvfb-run','-a','/output/prepass-gl','/output/prepass-gl.comp']
if failure:command+=['--expect-device-lost']
if mode in ['gl-queries','gl-wide-range']:command+=['--'+mode[3:]]
env={'PATH':'/usr/bin:/bin','HOME':'/tmp','VK_DRIVER_FILES':'/opt/shield/mesa/share/vulkan/icd.d/lvp_icd.x86_64.json','LP_NUM_THREADS':'2'}
if gl_mode:env.update({'XDG_RUNTIME_DIR':'/tmp','LIBGL_DRIVERS_PATH':'/opt/shield/mesa/lib/dri','LD_LIBRARY_PATH':'/opt/shield/mesa/lib','LIBGL_ALWAYS_SOFTWARE':'1','GALLIUM_DRIVER':'zink','MESA_LOADER_DRIVER_OVERRIDE':'zink','LIBGL_KOPPER_DRI2':'true','ZINK_DESCRIPTORS':'lazy'})
if mode=='gl-descriptor-buffer':env['ZINK_DESCRIPTORS']='db'
started=time.monotonic()
if no_route or variant:env['VK_INSTANCE_LAYERS']='VK_LAYER_KHRONOS_validation'
p=subprocess.run(command,env=env,preexec_fn=drop(1000),capture_output=True,text=True,timeout=40)
client_elapsed=time.monotonic()-started
report=json.loads(os.read(report_r,8192));os.close(report_r);_,status=os.waitpid(pid,0);done.set();thread.join(22)
expected_connections=8 if mode in ['masked','equivalent','gl-masked'] else 1 if mode in ['worker-corrupt','gl-worker-corrupt'] else 0
result={'passed':p.returncode==0 and os.waitstatus_to_exitcode(status)==0 and report.get('passed') and proxy_report['connections']==expected_connections and 'error' not in proxy_report,'mode':mode,'service':report,'proxy':proxy_report,'clientElapsedSeconds':client_elapsed,'clientExitCode':p.returncode,'client':p.stdout.strip(),'clientError':p.stderr.strip(),'scope':'patched standard Vulkan ICD in host container; no native VM/attestation claim'}
if not failure and not gl_mode and p.returncode==0:
 result['frameMatchesReference']=(Path(command[2]).read_bytes()==Path('/output/triangle.rgba').read_bytes());result['passed'] &= result['frameMatchesReference']
if mode in ['masked','equivalent','gl-masked']:result['passed'] &= report.get('telemetry',{}).get('verifiedOffloads')==8
if failure:result['passed'] &= client_elapsed < 5
if mode in ['worker-corrupt','gl-worker-corrupt']:result['passed'] &= proxy_report.get('modifiedRealGemmReply') is True and report.get('serviceRetired') is True
if no_route or variant:result['passed'] &= 'Validation Error' not in p.stdout+p.stderr and 'VUID-' not in p.stdout+p.stderr
print(json.dumps(result));assert result['passed']
