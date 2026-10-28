"""Loopback-only application/S3 integration; synthetic guest and public test keys."""
import argparse, contextlib, hashlib, http.server, json, os, pathlib, secrets, socket, struct, subprocess, sys, threading, time, urllib.request, urllib.error
parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--artifact',type=pathlib.Path,required=True)
parser.add_argument('--sha256',required=True)
parser.add_argument('--output-dir',type=pathlib.Path,required=True)
parser.add_argument('--set-runtime-dir',type=pathlib.Path,help='Retained SET runtime with its loader and libraries; enables codegen selftest')
parser.add_argument('--wasmtime',default='wasmtime',help='Ordinary WASI runtime when --set-runtime-dir is omitted')
args=parser.parse_args()
wasm=args.artifact.resolve()
assert hashlib.sha256(wasm.read_bytes()).hexdigest()==args.sha256,'artifact hash differs'
root=args.output_dir.resolve();root.mkdir(parents=True,exist_ok=True)
assert not any(root.iterdir()),'output directory must be empty'
if args.set_runtime_dir:
 runtime=args.set_runtime_dir.resolve()
 command=[str(runtime/'ld-linux-x86-64.so.2'),'--library-path',str(runtime),str(runtime/'wasmtime'),'run','-C','cache=n','-C','parallel-compilation=n','-W','max-memory-size=6442450944','-W','threads,shared-everything-threads,component-model-threading,shared-memory,memory64,component-model-memory64','-S','cli,tcp,inherit-network','-S','allow-ip-name-lookup=n','-S','udp=n','--env','ENCLAVE_AVAILABLE_PARALLELISM=1']
else:
 command=[args.wasmtime,'run','-S','nn','-S','inherit-network','-S','allow-ip-name-lookup=n','-S','udp=n']
http=urllib.request.build_opener(urllib.request.ProxyHandler({}))
# A tiny ELF guest increments a counter in RAM continuously; no Linux/network.
code=struct.pack('<4I',0x00002097,0x00128293,0x0050a023,0xff9ff06f)
elf=bytearray(b'\x7fELF\x02\x01\x01'+b'\0'*9)
elf+=struct.pack('<HHIQQQIHHHHHH',2,243,1,0x80000000,0,80,0,64,0,0,64,2,0)
elf+=code+b'\0'*64+struct.pack('<IIQQQQIIQQ',0,1,6,0x80000000,64,len(code),0,0,4,0)
objects={'/machines/kernel.elf':bytes(elf),'/machines/disk.bin':bytes(8192)}; puts=[]
class Store(http.server.BaseHTTPRequestHandler):
 def log_message(self,*a): pass
 def do_GET(self):
  data=objects.get(self.path)
  self.send_response(200 if data is not None else 404);self.send_header('Content-Length',str(len(data) if data is not None else 0));self.end_headers()
  if data is not None:self.wfile.write(data)
 def do_PUT(self):
  n=int(self.headers.get('Content-Length','0'));assert 0<n<8*1024**2
  data=self.rfile.read(n);assert len(data)==n
  objects[self.path]=data;puts.append((self.path,len(data)));self.send_response(200);self.send_header('Content-Length','0');self.end_headers()
server=http.server.ThreadingHTTPServer(('127.0.0.1',0),Store);threading.Thread(target=server.serve_forever,daemon=True).start()
token=secrets.token_hex(16)
config={'endpoint':f'http://127.0.0.1:{server.server_port}','bucket':'machines','kernel':'kernel.elf','fs':'disk.bin','net':False,'ramMiB':128,'display':{'width':320,'height':240},'api_key':token,'autostart':False,'snapshot':'private/session.snap','snapshotSaveKey':'private/session.snap','snapshotEncryption':{'masterKey':'42'*32,'owner':'test-owner','deployment':'test-deployment'}}
@contextlib.contextmanager
def app(cfg,label):
 with socket.socket() as sock:sock.bind(('127.0.0.1',0));port=sock.getsockname()[1]
 env=dict(PATH='/usr/sbin:/usr/bin:/bin',ENCLAVE_CONFIG=json.dumps(cfg),ENCLAVE_PORTS=f'http:8000={port}')
 log=(root/(label+'.log')).open('w')
 cmd=command+['--env','ENCLAVE_CONFIG','--env','ENCLAVE_PORTS',str(wasm)]
 proc=subprocess.Popen(cmd,env=env,stdout=log,stderr=subprocess.STDOUT)
 def req(path,body=None,auth=True):
  headers={'Authorization':'Bearer '+token} if auth else {}
  if body is not None:headers['Content-Type']='application/json'
  r=urllib.request.Request(f'http://127.0.0.1:{port}'+path,data=json.dumps(body).encode() if body is not None else None,headers=headers)
  try:
   with http.open(r,timeout=8) as res:return res.status,json.load(res)
  except urllib.error.HTTPError as e:return e.code,json.load(e)
 def phase(want):
  last=None;end=time.monotonic()+90
  while time.monotonic()<end:
   if proc.poll() is not None:raise AssertionError('app exited '+str(proc.returncode))
   try:
    _,last=req('/status')
    if last.get('phase')==want:return last
   except (OSError,ValueError):pass
   time.sleep(.1)
  raise AssertionError((want,last))
 try:
  phase('idle');yield req,phase
 finally:
  proc.terminate()
  try:proc.wait(timeout=3)
  except subprocess.TimeoutExpired:proc.kill();proc.wait(timeout=3)
  log.close()
checks=[]
def passed(name):checks.append(name);print('PASS',name,flush=True)
try:
 if args.set_runtime_dir:
  with (root/'codegen-selftest.log').open('w') as log:
   result=subprocess.run(command+['--env','RISC_CODEGEN_SELFTEST=1',str(wasm)],env={'PATH':'/usr/sbin:/usr/bin:/bin'},stdout=log,stderr=subprocess.STDOUT,timeout=90)
  assert result.returncode==0,'codegen selftest exited '+str(result.returncode)
  output=(root/'codegen-selftest.log').read_text()
  assert 'codegen main:' in output and 'codegen SET worker:' in output and output.count('PASS')==2 and 'FAIL' not in output
  passed('codegen main and SET worker execute and revoke generated code')
 with app(config,'private') as (req,phase):
  assert req('/snapshot',{},False)[0]==401;passed('snapshot endpoint requires owner authentication')
  assert req('/start',{'snapshot':False})[0] in (200,202);res=phase('running')
  if args.set_runtime_dir:assert res.get('jit') is not None and res['jit']['disabled'] is None,res.get('jit')
  status,result=req('/snapshot',{});assert status==200,(status,result)
  saved=objects['/machines/private/session.snap'];assert saved.startswith(b'EVMSNAP1') and result['encrypted'] is True
  assert sum(key=='/machines/private/session.snap' for key,_ in puts)==1;passed('actual S3 PUT contains authenticated envelope')
  req('/stop',{});phase('halted');req('/start',{'reset':True,'snapshot':True});res=phase('running');assert res['snapshot']['restored'] is True,res['snapshot'];passed('app fetches and accepts its saved snapshot through the restore path')
  req('/stop',{});phase('halted');objects['/machines/private/session.snap']=saved[:-1]+bytes([saved[-1]^1]);req('/start',{'reset':True,'snapshot':True});res=phase('error');assert 'authentication' in res['error'];passed('tampered S3 object fails closed instead of cold boot')
  req('/start',{'reset':True,'snapshot':False});phase('running');passed('explicit cold boot remains available after a damaged private snapshot')
 with app({k:v for k,v in config.items() if k!='snapshotEncryption'},'no-key') as (req,phase):
  req('/start',{'snapshot':False});phase('running');before=sum(k=='/machines/private/session.snap' for k,_ in puts);assert req('/snapshot',{})[0]==409;assert sum(k=='/machines/private/session.snap' for k,_ in puts)==before;passed('missing snapshot key prevents storage upload')
 with app(dict(config,snapshotEncryption=dict(config['snapshotEncryption'],owner='other-owner')),'wrong-owner') as (req,phase):
  objects['/machines/private/session.snap']=saved
  req('/start',{'snapshot':True});res=phase('error');assert 'authentication' in res['error'];passed('wrong owner cannot restore through the app')
finally:
 server.shutdown();server.server_close()
 (root/'puts.json').write_text(json.dumps([{'path':k,'bytes':n} for k,n in puts],indent=2)+'\n')
(root/'receipt.json').write_text(json.dumps({'passed':checks,'count':len(checks),'artifactSha256':hashlib.sha256(wasm.read_bytes()).hexdigest(),'setRuntime':bool(args.set_runtime_dir),'scope':'Synthetic RISC guest with loopback S3 fixture. No production traffic or real credentials. Runtime network capability is broad (CLI has no address allowlist); fixture URLs are pinned to loopback, DNS/UDP disabled, and child environment has no host credentials.'},indent=2)+'\n')
