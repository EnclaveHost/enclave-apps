"""KVM-only immutable image graph with the owner page, its terminal, the desktop link and the pairing route
with nothing to pair; real model hashes,
synthetic worker; no GPU or admission claim."""
import base64,ctypes,hashlib,http.client,json,os,re,signal,socket,struct,subprocess,threading,time,traceback
from pathlib import Path
assert os.getpid()==1 and os.getuid()==0 and os.uname().release=='6.6.87.2-enclave-kvm-box'
AUTH='Basic '+base64.b64encode(b'owner:synthetic-control-chain-owner').decode()
procs=[];logs=[];checks=[];facts={};worker_count=0;worker_fault=None
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
  c.sock=connect();c.request(method,path,headers=h);r=c.getresponse();return r.status,r.read(4<<20),dict(r.getheaders())
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
def term(body,auth=True,action=True,headers=None):
 h={'Host':'fixture.test','Connection':'close','Content-Type':'application/json'}
 if auth:h['Authorization']=AUTH
 if action:h['X-KVM-Box-Request']='1'
 h.update(headers or {})
 c=http.client.HTTPConnection('127.0.0.1',8000,timeout=8)
 try:
  c.sock=connect();c.sock.settimeout(8);c.request('POST','/terminal',json.dumps(body),h);r=c.getresponse();return r.status,r.read(1<<20)
 finally:c.close()
def tcall(body):
 code,raw=term(body);assert code==200,(code,raw[:300]);s=json.loads(raw);assert s['ok'] is True,s;return s
def ttype(data):tcall({'op':'write','data':base64.b64encode(data).decode()})
def texpect(cursor,pattern,seconds=25):
 buf=b'';end=time.monotonic()+seconds
 while time.monotonic()<end:
  s=tcall({'op':'read','cursor':cursor,'wait_ms':1500} if cursor is not None else {'op':'read','wait_ms':0});buf+=base64.b64decode(s['data']);cursor=s['cursor']
  m=re.search(pattern,buf)
  if m:return cursor,m,s
 raise AssertionError(('terminal output deadline',pattern,buf[-400:]))
def terminal_checks():
 probe={'op':'read','wait_ms':0}
 assert term(probe,auth=False)[0]==401
 assert term(probe,action=False)[0]==403
 # The automation key is for /exec and /computer; it does not open the owner's terminal.
 assert term(probe,auth=False,headers={'X-Api-Key':'synthetic-wx-automation-key-1234567890'})[0]==401
 mark('terminal-owner-and-page-action-gates')
 s=tcall(probe);assert s['reset'] is True,s;cursor=s['cursor']
 # Typed text is echoed with its $ signs; only the shell's own output matches.
 ttype(b'echo "uid=$(id -u) gid=$(id -g) cwd=$PWD term=$TERM sum=$((40+2))"\n')
 cursor,m,_=texpect(cursor,rb'uid=(\d+) gid=(\d+) cwd=(\S+) term=(\S+) sum=42\r')
 assert m.groups()==(b'1000',b'1000',b'/home/desktop',b'xterm-256color'),m.groups()
 mark('terminal-shell-is-the-desktop-user')
 tcall({'op':'resize','cols':120,'rows':30});ttype(b'stty size\n')
 cursor,m,s=texpect(cursor,rb'[\r\n]30 120\r\n');assert (s['cols'],s['rows'])==(120,30),s
 mark('terminal-resize-reaches-the-pty')
 ttype(b'sleep 300\n');time.sleep(.6);ttype(b'\x03');ttype(b'echo after=$((20+3))\n')
 cursor,m,_=texpect(cursor,rb'after=23\r',10);mark('terminal-interrupt-key')
 ttype(b'echo shell=$$\n');cursor,m,_=texpect(cursor,rb'shell=(\d+)\r');shell=int(m.group(1))
 # A second page (no cursor) gets the same session from its scrollback.
 s=tcall(probe);assert s['reset'] is True and b'sum=42' in base64.b64decode(s['data']) and s['cursor']>=cursor,(s['cursor'],cursor)
 mark('terminal-second-page-sees-the-session')
 tcall({'op':'restart'})
 def typed():
  code,raw=term({'op':'write','data':base64.b64encode(b'echo shell=$$\n').decode()})
  return code==200 and json.loads(raw).get('ok') is True
 until(typed,8)
 cursor,m,s=texpect(cursor,rb'shell=(\d+)\r');assert int(m.group(1))!=shell and s['running'] is True
 mark('terminal-restart-is-a-new-shell')
 return shell
def post(path,body,auth=True,action=True,headers=None):
 h={'Host':'fixture.test','Connection':'close','Content-Type':'application/json'}
 if auth:h['Authorization']=AUTH
 if action:h['X-KVM-Box-Request']='1'
 h.update(headers or {})
 c=http.client.HTTPConnection('127.0.0.1',8000,timeout=8)
 try:
  c.sock=connect();c.sock.settimeout(8);c.request('POST',path,json.dumps(body),h);r=c.getresponse();return r.status,r.read(1<<20)
 finally:c.close()
def pairing_checks():
 # The gateway's own question about the paired records is no web route: the owner's sign-in, the page header
 # and the automation key all get what a path nobody serves gets, and never a record.
 key={'X-Api-Key':'synthetic-wx-automation-key-1234567890'}
 # Asked of the controller (/paired) and, through it, of the desktop's gateway (/desktop/paired).
 for at in ['','/desktop']:
  none=post(at+'/no-such-route',{})[0];assert none!=200,none
  for kw in [{},{'action':False},{'headers':key},{'auth':False,'headers':key},{'auth':False}]:
   code,raw=post(at+'/paired',{},**kw);assert code!=200 and b'cert_sha256' not in raw and b'clients' not in raw,(at,kw,code)
  assert post(at+'/paired',{})[0]==none;facts['pairedFromTheWeb'+at]=none
 mark('paired-records-question-has-no-web-route')
 # A PIN nobody asked for. 409 is the gateway's answer once it has read its credential and Sunshine has
 # answered that no client waits for a PIN; nothing is stored.
 assert post('/desktop/api/moonlight/pair',{'pin':'0000'},auth=False)[0]==401
 assert post('/desktop/api/moonlight/pair',{'pin':'0000'},action=False)[0]==403
 def asked():
  code,raw=post('/desktop/api/moonlight/pair',{'pin':'0000'});facts['unaskedPin']=code
  return (code,raw) if code==409 else None
 code,raw=until(asked,40);assert json.loads(raw).get('status') is False,raw[:200]
 mark('pairing-route-reaches-sunshine-with-nothing-to-pair')
ok=False
try:
 def drop(uid):
  def child():os.setgroups([]);os.setgid(uid);os.setuid(uid)
  return child
 canary=subprocess.Popen(['synthetic-credential-canary','30'],executable='/usr/bin/sleep',preexec_fn=drop(1000),stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 try:
  for uid in [1002,7501,1056077]:
   code="import os,sys\ntry:\n open('/proc/"+str(canary.pid)+"/cmdline','rb').read()\nexcept (FileNotFoundError,PermissionError): sys.exit(0)\nelse: sys.exit(1)"
   observer=subprocess.run(['/usr/bin/python3','-I','-c',code],preexec_fn=drop(uid),capture_output=True,timeout=3)
   assert observer.returncode==0,('ancestor cmdline visible',uid)
 finally:canary.terminate();canary.wait(3)
 mark('ancestor-proc-hides-credential-argv')
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
 code,b,headers=request('/desktop/vnc.html');assert code==200 and b'noVNC' in b;mark('real-desktop-vnc-assets')
 code,page,_=request('/');assert code==200
 for part in [b'href="/desktop/vnc.html?autoconnect=true&resize=remote&path=/desktop/websockify"',b"running:'Stop desktop'",b"stopped:'Start desktop'",b'id="power"',b'id="term"',b"fetch('/terminal'",b'<span id="ml-public" hidden>In Moonlight, add PC <strong id="ml-name"></strong>',b'<span id="ml-local">',b"$('#ml-name').textContent=location.hostname"]:assert part in page,part
 assert b'path=desktop/websockify' not in page and b'id="start"' not in page and b'id="stop"' not in page
 facts['pageSha256']=hashlib.sha256(page).hexdigest()
 assert request('/a/xterm.js',auth=False)[0]==401
 for name in ['xterm.js','xterm.css']:
  code,b,headers=request('/a/'+name);assert code==200 and int(headers['Content-Length'])==len(b),(name,code)
  facts[name]={'bytes':len(b),'sha256':hashlib.sha256(b).hexdigest()}
 mark('owner-page-one-power-button-terminal-assets');mark('owner-page-names-the-public-moonlight-pc')
 # The address the page's Open desktop link gives noVNC: a WebSocket to the desktop's VNC server.
 key=base64.b64encode(os.urandom(16)).decode()
 s,f,headers=upgraded('/desktop/websockify','websocket',f'Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: binary\r\n')
 try:
  accept=base64.b64encode(hashlib.sha1((key+'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').encode()).digest())
  assert headers.get(b'sec-websocket-accept')==accept
  b0,b1=exact(f,2);assert b0&0x0f in (1,2) and not b1&0x80 and 0<(b1&0x7f)<126,(b0,b1)
  banner=exact(f,b1&0x7f);assert banner.startswith(b'RFB 003.'),banner;facts['vncBanner']=banner.decode().strip()
 finally:f.close();s.close()
 mark('open-desktop-link-websocket-reaches-vnc')
 code="""import ctypes,errno,json
c=ctypes.CDLL(None,use_errno=True);c.mmap.restype=ctypes.c_void_p;c.mmap.argtypes=[ctypes.c_void_p,ctypes.c_size_t,ctypes.c_int,ctypes.c_int,ctypes.c_int,ctypes.c_long]
p=c.mmap(None,4096,7,0x22,-1,0)
assert p==ctypes.c_void_p(-1).value and ctypes.get_errno()==errno.EPERM
from pathlib import Path
prefs=Path('/home/desktop/.mozilla/firefox/kvm-box/user.js').read_text()
assert 'user_pref("javascript.options.content_process_write_protect_code", true);' in prefs
print(json.dumps({'passed':True,'rwxDenied':True,'firefoxWriteProtection':True}))
"""
 import shlex
 c=http.client.HTTPConnection('127.0.0.1',8000,timeout=30);c.sock=connect()
 body=json.dumps({'cmd':'python3 -c '+shlex.quote(code),'timeout_s':15})
 c.request('POST','/exec',body,{'Host':'fixture.test','X-Api-Key':'synthetic-wx-automation-key-1234567890','Content-Type':'application/json','Connection':'close'})
 r=c.getresponse();raw=r.read(65536);c.close();assert r.status==200,(r.status,raw)
 result=json.loads(raw);assert result['exitCode']==0,result;assert json.loads(result['output'])['passed'];mark('automation-inherits-RWX-denial-and-Firefox-default')
 assert status()['status']=='running' and status()['pid']==first['pid'];mark('desktop-survives-denied-allocation')
 terminal_checks()
 assert status()['status']=='running' and status()['pid']==first['pid']
 pairing_checks()
 assert status()['status']=='running' and status()['pid']==first['pid']
 assert request('/api/stop','POST')[0]==200;until(lambda:status()['status']=='stopped',15);mark('clean-desktop-stop')
 code,raw=term({'op':'read','wait_ms':0});facts['terminalWhileStopped']=code
 assert code!=200 or json.loads(raw).get('ok') is not True,(code,raw[:200]);mark('terminal-ends-with-the-desktop')
 assert request('/api/start','POST')[0]==200;until(lambda:status()['status']=='running')
 second=status();assert second['pid']!=first['pid']
 def fresh():
  code,raw=term({'op':'read','wait_ms':0})
  s=json.loads(raw) if code==200 else {}
  return s if s.get('ok') is True else None
 s=until(fresh,30);assert s['reset'] is True and b'sum=42' not in base64.b64decode(s['data'])
 ttype(b'echo again=$((50+5))\n');texpect(s['cursor'],rb'again=55\r');mark('terminal-starts-fresh-with-the-next-desktop')
 assert request('/api/stop','POST')[0]==200;until(lambda:status()['status']=='stopped',15);mark('second-clean-desktop-stop')
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
 print('CONTROL_GRAPH_RESULT='+json.dumps({'passed':ok,'checks':checks,'facts':facts,'workerProbes':worker_count,'workerError':worker_fault,'gpu':False,'authenticatedResources':False,'productionAdmission':False,'moonlightStreaming':False}),flush=True)
 os.sync();ctypes.CDLL(None).reboot(0x4321fedc)
