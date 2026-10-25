"""KVM-only full control graph functional fixture; no GPU or admission claim."""
import base64,ctypes,hashlib,http.client,json,os,signal,socket,struct,subprocess,threading,time,traceback
from pathlib import Path
assert os.getpid()==1 and os.getuid()==0 and os.uname().release=='6.6.87.2-enclave-kvm-box'
AUTH='Basic '+base64.b64encode(b'owner:synthetic-control-chain-owner').decode()
procs=[];logs=[];checks=[];worker_count=0;worker_fault=None
worker=socket.socket(fileno=3);worker.settimeout(.2);done=threading.Event()
def worker_loop():
 global worker_count,worker_fault
 while not done.is_set():
  try:c,_=worker.accept()
  except socket.timeout:continue
  except OSError:break
  try:
   _,uid,_=struct.unpack('3i',c.getsockopt(socket.SOL_SOCKET,socket.SO_PEERCRED,12))
   if uid!=1001:raise AssertionError('unexpected worker peer')
   c.settimeout(1)
   if c.recv(1):raise AssertionError('lifecycle probe sent worker protocol data')
   worker_count+=1
  except Exception as e:worker_fault=type(e).__name__
  finally:c.close()
def launch(args,name):
 f=open('/run/'+name+'.log','wb');logs.append((name,f));p=subprocess.Popen(args,stdin=subprocess.DEVNULL,stdout=f,stderr=f,env={'PATH':'/usr/bin:/bin','LANG':'C.UTF-8'},close_fds=True);procs.append(p);return p
def connect():
 os.seteuid(1056077)
 try:return socket.create_connection(('127.0.0.1',8000),3)
 finally:os.seteuid(0)
def request(path,method='GET',auth=True,action=True,headers=None):
 h={'Host':'fixture.test','Connection':'close'}
 if auth:h['Authorization']=AUTH
 if method=='POST' and action:h['X-KVM-Box-Request']='1'
 h.update(headers or {})
 c=http.client.HTTPConnection('127.0.0.1',8000,timeout=5)
 try:
  c.sock=connect();c.request(method,path,headers=h);r=c.getresponse();return r.status,r.read(1<<20),dict(r.getheaders())
 finally:c.close()
def status():
 code,body,_=request('/api/status');assert code==200;return json.loads(body)
def until(fn,seconds=65):
 end=time.monotonic()+seconds
 while time.monotonic()<end:
  for p in procs:assert p.poll() is None,('process exit',p.args[0],p.returncode)
  try:
   value=fn()
   if value:return value
  except (OSError,http.client.HTTPException):pass
  time.sleep(.05)
 raise AssertionError('condition deadline')
def mark(s):checks.append(s);print('CONTROL_GRAPH_PASS '+s,flush=True)
def exact(f,n):
 b=f.read(n);assert len(b)==n;return b
def upgraded(path,kind,extra=''):
 s=connect();s.settimeout(5)
 s.sendall((f'GET {path} HTTP/1.1\r\nHost: fixture.test\r\nAuthorization: {AUTH}\r\nConnection: Upgrade\r\nUpgrade: {kind}\r\n'+extra+'\r\n').encode())
 f=s.makefile('rb');first=f.readline(4096);assert first.startswith(b'HTTP/1.1 101 '),first
 headers={}
 while True:
  line=f.readline(4096);assert line
  if line==b'\r\n':break
  k,v=line.split(b':',1);headers[k.lower()]=v.strip()
 assert headers.get(b'upgrade',b'').lower()==kind.encode()
 return s,f,headers
ok=False
try:
 thread=threading.Thread(target=worker_loop,daemon=True);thread.start()
 sup=launch(['/supervisor','2','8192'],'supervisor')
 until(lambda:Path('/run/native-desktop/control.sock').is_socket(),5)
 launch(['/probe-gateway'],'gateway');launch(['/probe-runtime'],'runtime')
 until(lambda:request('/api/status')[0]==200,25)
 assert status()['status']=='stopped'
 assert request('/api/start','POST',auth=False)[0]==401
 assert request('/api/start','POST',action=False)[0]==403
 assert request('/desktop/v1/start','POST')[0]==404
 assert status()['status']=='stopped';mark('owner-auth-and-action-gates')
 assert request('/api/start','POST')[0]==200
 until(lambda:status()['status']=='running')
 first=status();assert first['vcpus']==2 and first['memoryMiB']==8192 and first['pid']>1
 assert request('/api/start','POST')[0]==200 and status()['pid']==first['pid'];mark('real-supervisor-nsinit-start')
 code,b,h=request('/desktop/vnc.html');assert code==200 and h.get('Content-Type','').startswith('text/html') and b'noVNC' in b
 code,b,h=request('/desktop/compute/api/native');v=json.loads(b);assert code==200 and v['ok'] is True and v['state']=='ready' and v['engines']==['llama','sd'];mark('actual-static-and-compute-route')
 s,f,h=upgraded('/desktop/websockify','websocket','Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Protocol: binary\r\n')
 try:
  assert h.get(b'sec-websocket-accept')==b's3pPLMBiTxaQ9kYGzzhZRbK+xOo='
  frame=exact(f,2);assert frame[0]&15==2 and not frame[1]&128
  length=frame[1]&127
  if length==126:length=struct.unpack('!H',exact(f,2))[0]
  assert 1<=length<=4096
  assert exact(f,length).startswith(b'RFB ')
 finally:f.close();s.close()
 mark('actual-websocket-to-x11vnc-rfb')
 s,f,h=upgraded('/desktop/api/moonlight/tunnel?transport=tcp&port=47989','kvm-box-tunnel')
 try:
  s.sendall(b'GET /serverinfo?uniqueid=probe HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n')
  response=f.read(65536);assert response.startswith(b'HTTP/1.1 200 ') and b'<hostname>KVM Box</hostname>' in response
 finally:f.close();s.close()
 mark('actual-sunshine-serverinfo-tunnel')
 for uid,port in [(1056077,19080),(1056077,18080),(7501,18080),(7501,5900),(7501,47989)]:
  os.seteuid(uid)
  try:
   try:c=socket.create_connection(('127.0.0.1',port),.5)
   except OSError:continue
   c.close();raise AssertionError(('direct bypass accepted',uid,port))
  finally:os.seteuid(0)
 mark('front-and-runtime-direct-bypass-refused')
 assert request('/api/stop','POST')[0]==200
 until(lambda:status()['status']=='stopped',8)
 assert not Path('/proc/'+str(first['pid'])).exists();mark('supervisor-clean-stop')
 assert request('/api/start','POST')[0]==200
 until(lambda:status()['status']=='running')
 assert status()['pid']!=first['pid']
 assert request('/desktop/compute/api/native')[0]==200
 assert request('/api/stop','POST')[0]==200
 until(lambda:status()['status']=='stopped',8)
 assert worker_count>=2 and worker_fault is None;mark('fresh-desktop-restart')
 assert request('/api/start','POST')[0]==200
 until(lambda:status()['status']=='running')
 last=status()['pid']
 descendants=[int(x) for x in Path(f'/proc/{last}/task/{last}/children').read_text().split()]
 assert len(descendants)==10
 sup.kill();sup.wait(3)
 deadline=time.monotonic()+5
 while time.monotonic()<deadline:
  try:
   while True:
    child,_=os.waitpid(-1,os.WNOHANG)
    if child==0:break
  except ChildProcessError:pass
  if not any(Path('/proc/'+str(pid)).exists() for pid in [last]+descendants):break
  time.sleep(.025)
 assert not any(Path('/proc/'+str(pid)).exists() for pid in [last]+descendants)
 assert request('/api/status')[0]==503  # supervisor unavailable; desktop proxy failure is 502
 assert request('/desktop/compute/api/native')[0]==502
 assert worker_count>=3 and worker_fault is None;mark('supervisor-death-removes-desktop-tree')
 ok=True
except Exception:
 traceback.print_exc()
finally:
 done.set();worker.close()
 for p in reversed(procs):
  if p.poll() is None:
   p.terminate()
   try:p.wait(6)
   except subprocess.TimeoutExpired:p.kill();p.wait(3)
 for name,f in logs:
  f.close()
  # Test runtime diagnostics contain only synthetic owner data, never production credentials.
  if not ok:
   data=Path('/run/'+name+'.log').read_text(errors='replace')[-6000:]
   print('CONTROL_GRAPH_DIAGNOSTIC '+name+'\n'+data,flush=True)
 print('CONTROL_GRAPH_RESULT='+json.dumps({'passed':ok,'checks':checks,'workerProbes':worker_count,'workerError':worker_fault,'gpu':False,'authenticatedResources':False,'productionAdmission':False,'moonlightStreaming':False}),flush=True)
 os.sync();ctypes.CDLL(None).reboot(0x4321fedc)
