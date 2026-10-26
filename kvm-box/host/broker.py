#!/usr/bin/env python3
"""Dedicated SNP VM broker. No arbitrary image paths, commands, resource sizes, or plain-VM fallback."""
import argparse,base64,hashlib,http.server,json,os,re,secrets,signal,socket,socketserver,subprocess,threading,time
from pathlib import Path

def deployment_host_data(deployment_id=None,development_unbound=False):
 """Use Enclave's deployment bytes32 verbatim; this is not wallet authorization."""
 if development_unbound:
  if deployment_id is not None:raise ValueError('Deployment binding and development-unbound are mutually exclusive')
  return None
 if not isinstance(deployment_id,str) or not re.fullmatch(r'0x[0-9a-fA-F]{64}',deployment_id):
  raise ValueError('Provide --deployment-id with a nonzero 0x-prefixed bytes32, or explicitly --development-unbound')
 value=deployment_id[2:].lower()
 if value=='0'*64:raise ValueError('An all-zero deployment id names no deployment')
 return value

def snp_guest_object(host_data):
 value='sev-snp-guest,id=sev0,cbitpos=51,reduced-phys-bits=1,kernel-hashes=on'
 if host_data is not None:
  checked=deployment_host_data('0x'+host_data)
  value+=',host-data='+base64.b64encode(bytes.fromhex(checked)).decode('ascii')
 return value

def sha(path):
 h=hashlib.sha256()
 with open(path,'rb') as f:
  for b in iter(lambda:f.read(1<<20),b''):h.update(b)
 return h.hexdigest()
class VM:
 def __init__(self,broker,name):
  self.b=broker;self.name=name;self.spec=broker.release['images'][name];self.unit=f'kvm-box-{name}-{broker.tag}.service';self.cid=broker.cids[name];self.lock=threading.RLock();self.generation=0;self.status='stopped';self.error=None;self.fwd=None;self.port=None;self.worker=None;self.stop_worker=None;self.cancel=threading.Event()
 def state(self):
  with self.lock:
   if self.status=='running' and subprocess.run(['systemctl','--user','is-active','--quiet',self.unit]).returncode:self.status='failed';self.error='Guest exited'
   return {'name':self.name,'status':self.status,'error':self.error,'backend':'kvm-sev-snp','architecture':'x86_64','vcpus':self.spec['vcpus'],'memoryMiB':self.spec['memoryMiB'],'measurement':self.spec['measurement'],'appId':self.spec['appId'],'hostPort':self.port,'cid':self.cid,'storage':'ephemeral','hostData':self.b.host_data,'bindingMode':'deployment-id' if self.b.host_data else 'development-unbound'}
 def start(self):
  with self.lock:
   if self.status in ('starting','running','stopping'):return self.state()
   self.generation+=1;generation=self.generation;self.status='starting';self.error=None;self.cancel.clear()
   self.worker=threading.Thread(target=self._start,args=(generation,),daemon=True);self.worker.start();self.b.save();return self.state()
 def current(self,generation):
  with self.lock:return generation==self.generation
 def _start(self,generation):
  try:
   self.b.check_files(self.name)
   s=self.spec;r=self.b.release;root=self.b.root
   cmd=['systemd-run','--user','--quiet','--collect','--unit',self.unit,'-p',f'MemoryMax={s["memoryMiB"]+1024}M','-p',f'CPUQuota={s["vcpus"]*100}%','-p','TasksMax=256']
   dependencies=([self.b.parent_unit] if self.b.parent_unit else [])+([self.b.vms['controller'].unit] if self.name=='desktop' else [])
   if dependencies:cmd+=['-p','BindsTo='+' '.join(dependencies),'-p','After='+' '.join(dependencies)]
   cmd+=['qemu-system-x86_64','-machine','q35,accel=kvm,confidential-guest-support=sev0,memory-backend=ram1','-object',snp_guest_object(self.b.host_data),'-object',f'memory-backend-memfd,id=ram1,size={s["memoryMiB"]}M,share=true','-cpu','host','-smp',str(s['vcpus']),'-m',str(s['memoryMiB'])+'M','-bios',str(root/r['firmware']['file']),'-kernel',str(root/r['kernel']['file']),'-initrd',str(root/s['image']),'-append',r['commandLine'],'-device',f'vhost-vsock-pci,guest-cid={self.cid}','-nodefaults','-display','none','-serial','file:'+str(self.b.work/(self.name+'.serial')),'-no-reboot']
   if s['network']:cmd+=['-netdev','user,id=net0','-device','virtio-net-pci,netdev=net0,disable-legacy=on,iommu_platform=on']
   for i,volume in enumerate(s.get('volumes',[])):
    cmd+=['-drive',f'file={root/volume["file"]},if=none,id=model{i},format=raw,readonly=on',
          '-device',f'virtio-blk-pci,drive=model{i},disable-legacy=on,iommu_platform=on']
   subprocess.run(cmd,check=True,capture_output=True,text=True,timeout=15)
   if not self.current(generation):self._cleanup();return
   fwdlog=open(self.b.work/(self.name+'-fwd.log'),'w')
   fwd=subprocess.Popen([str(self.b.work/'fwd'),'-cid',str(self.cid)],stdout=subprocess.PIPE,stderr=fwdlog,text=True)
   with self.lock:self.fwd=fwd
   line=fwd.stdout.readline();m=re.search(r'FWD listening 127.0.0.1:(\d+)',line)
   if not m:raise RuntimeError('TLS forwarder failed')
   port=int(m[1]);fwdlog.write(line);fwdlog.flush()
   # Drain forwarder logs to keep its pipe from blocking during interactive use.
   def drain():
    try:
     for ln in fwd.stdout:fwdlog.write(ln);fwdlog.flush()
    finally:fwdlog.close()
   threading.Thread(target=drain,daemon=True).start()
   with self.lock:self.port=port
   self.b.save()
   args=['node',str(self.b.enclave/'isolation/m2/client.mjs'),f'https://127.0.0.1:{port}','--measurement',s['measurement'],'--app-sha',s['appId'],'--no-kds','--vcek',str(self.b.vcek),'--amd-chain','Turin='+str(self.b.chain),'--min-tcb','@'+str(self.b.tcb),'--answer-within','180000','--save',str(self.b.work/(self.name+'-attestation.json'))]
   if self.name=='controller':args+=['--runtime',str(root/'controller-runtime.json')]
   if self.b.host_data is not None:args+=['--host-data',self.b.host_data]
   p=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
   deadline=time.monotonic()+200
   while p.poll() is None and not self.cancel.wait(1) and time.monotonic()<deadline:
    if subprocess.run(['systemctl','--user','is-active','--quiet',self.unit]).returncode:break
   if p.poll() is None:p.terminate()
   output,_=p.communicate(timeout=5)
   (self.b.work/(self.name+'-verify.log')).write_text(output)
   if p.returncode or 'VERDICT attested' not in output or 'RESULT gate=open' not in output:raise RuntimeError('Attestation failed; see '+self.name+'-verify.log')
   if self.b.host_data is not None and not {'RESULT host_data_checked=1','RESULT host_data='+self.b.host_data}.issubset(output.splitlines()):raise RuntimeError('Verifier did not confirm the expected deployment binding')
   with self.lock:
    if generation!=self.generation:self._cleanup();return
    self.status='running'
   self.b.save()
  except Exception as e:
   self._cleanup()
   with self.lock:
    if generation==self.generation:self.status='failed';self.error=str(e)
   self.b.save()
 def _cleanup(self):
  subprocess.run(['systemctl','--user','stop',self.unit],capture_output=True,timeout=20)
  with self.lock:
   if self.fwd:
    self.fwd.terminate()
    try:self.fwd.wait(timeout=5)
    except subprocess.TimeoutExpired:self.fwd.kill();self.fwd.wait()
    self.fwd=None
   self.port=None
 def request_stop(self):
  with self.lock:
   if self.status in ('stopped','stopping'):return self.state()
   self.generation+=1;self.status='stopping';self.cancel.set();worker=self.worker
   self.stop_worker=threading.Thread(target=self._finish_stop,args=(worker,),daemon=True);self.stop_worker.start()
   return self.state()
 def _finish_stop(self,worker):
  if worker and worker is not threading.current_thread():worker.join(timeout=220)
  self._cleanup()
  with self.lock:self.status='stopped';self.error=None
  self.b.save()
 def stop(self):
  self.request_stop()
  with self.lock:worker=self.stop_worker
  if worker and worker is not threading.current_thread():worker.join(timeout=240)
  return self.state()

class Broker:
 def __init__(self,a):
  self.host_data=deployment_host_data(getattr(a,'deployment_id',None),getattr(a,'development_unbound',False))
  self.root=a.release.resolve();self.work=a.work.resolve();self.work.mkdir(parents=True,exist_ok=True);self.work.chmod(0o700);self.enclave=a.enclave.resolve();self.parent_unit=a.parent_unit
  self.release=json.loads((self.root/'release.json').read_text());self.tag=secrets.token_hex(4);self.cids={'controller':0x20000+secrets.randbelow(50000),'desktop':0x30000+secrets.randbelow(50000)}
  self.vcek=a.vcek;self.chain=a.chain;self.tcb=a.min_tcb;self.save_lock=threading.Lock()
  self.vms={n:VM(self,n) for n in ['controller','desktop']}
 def check_files(self,name):
  for item in [self.release['kernel'],self.release['firmware'],{'file':self.release['images'][name]['image'],'sha256':self.release['images'][name]['sha256']},*self.release['images'][name].get('volumes',[])]:
   if Path(item['file']).name!=item['file'] or sha(self.root/item['file'])!=item['sha256']:raise ValueError('Release digest mismatch: '+item['file'])
 def save(self):
  # Snapshot without nested per-VM locks: save can be invoked during either VM's state change.
  with self.save_lock:
   d={n:{'generation':v.generation,'status':v.status,'error':v.error,'hostPort':v.port,'cid':v.cid,'unit':v.unit,'hostData':self.host_data,'bindingMode':'deployment-id' if self.host_data else 'development-unbound'} for n,v in self.vms.items()}
   tmp=self.work/'state.tmp';tmp.write_text(json.dumps(d,indent=2));tmp.replace(self.work/'state.json')
 def close(self):
  for v in self.vms.values():v.stop()
class Server(socketserver.ThreadingMixIn,socketserver.TCPServer):
 address_family=socket.AF_VSOCK;daemon_threads=True
 def verify_request(self,request,client_address):return client_address[0]==self.broker.cids['controller']
class Handler(http.server.BaseHTTPRequestHandler):
 protocol_version='HTTP/1.1'
 def log_message(self,*args):pass
 def do_GET(self):self.handle_api()
 def do_POST(self):self.handle_api()
 def handle_api(self):
  self.connection.settimeout(5)
  if self.headers.get('Transfer-Encoding') or self.headers.get('Content-Length','0')!='0':self.send_error(400);return
  v=self.server.broker.vms['desktop'];status=200
  if self.command=='GET' and self.path=='/v1/status':body=v.state()
  elif self.command=='POST' and self.path=='/v1/start':body=v.start();status=202
  elif self.command=='POST' and self.path=='/v1/stop':
   body=v.request_stop();status=202
  else:self.send_error(404);return
  data=json.dumps(body).encode();self.send_response(status);self.send_header('Content-Type','application/json');self.send_header('Content-Length',str(len(data)));self.send_header('Connection','close');self.end_headers();self.wfile.write(data);self.close_connection=True
 def setup(self):super().setup();self.connection.settimeout(5)
def main():
 ap=argparse.ArgumentParser(allow_abbrev=False);ap.add_argument('--release',type=Path,required=True);ap.add_argument('--work',type=Path,required=True);ap.add_argument('--enclave',type=Path,required=True);ap.add_argument('--vcek',type=Path,required=True);ap.add_argument('--chain',type=Path,required=True);ap.add_argument('--min-tcb',type=Path,required=True);ap.add_argument('--parent-unit',default='')
 binding=ap.add_mutually_exclusive_group(required=True);binding.add_argument('--deployment-id');binding.add_argument('--development-unbound',action='store_true')
 import sys
 if sum(x=='--deployment-id' or x.startswith('--deployment-id=') for x in sys.argv[1:])>1 or sys.argv.count('--development-unbound')>1:ap.error('Repeated binding arguments are refused')
 a=ap.parse_args();b=Broker(a)
 server=Server((socket.VMADDR_CID_ANY,19080),Handler);server.broker=b
 def end(*_):raise KeyboardInterrupt
 signal.signal(signal.SIGTERM,end)
 try:
  b.vms['controller'].start();server.serve_forever(poll_interval=.5)
 except KeyboardInterrupt:pass
 finally:server.server_close();b.close()
if __name__=='__main__':main()
