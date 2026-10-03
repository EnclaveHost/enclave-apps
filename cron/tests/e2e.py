#!/usr/bin/env python3
"""Real command-component test: HTTP API, encrypted CAS storage, autonomous dispatch,
MCP, per-user boundaries, crash recovery and lost-write fencing. Synthetic data only.
Runs the same transport/engine as production; no GPU/model or live user data used.
"""
import sys,argparse,base64,hashlib,hmac,json,os,socket,subprocess,tempfile,threading,time,urllib.request,urllib.error
from http.server import BaseHTTPRequestHandler,ThreadingHTTPServer
from pathlib import Path
from datetime import datetime,timezone
from cryptography.hazmat.primitives.ciphers.aead import AESGCM
ROOT=Path(__file__).resolve().parents[1]
KEY='test-service-key-'+('a'*40);MASTER='test-encryption-key-'+('b'*40)
USER='acct_'+('a'*32);OTHER='acct_'+('b'*32)
ACCESS='synthetic-access';SECRET='synthetic-storage-secret'
blob=None;etag=None;calls=[];lock=threading.Lock();fail_put=False;lose_put_reply=False

def seal_state(state):
    k=hmac.new(hashlib.sha256(MASTER.encode()).digest(),b'enclave-cron-v1:state',hashlib.sha256).digest()
    n=os.urandom(12)
    return b'CRN1'+n+AESGCM(k).encrypt(n,json.dumps(state).encode(),b'cron/state')
def opened():
    k=hmac.new(hashlib.sha256(MASTER.encode()).digest(),b'enclave-cron-v1:state',hashlib.sha256).digest()
    return json.loads(AESGCM(k).decrypt(blob[4:16],blob[16:],b'cron/state'))
def expire_lease():
    global blob,etag
    with lock:
        s=opened();s['lease_until']=0;blob=seal_state(s);etag='"'+hashlib.sha256(blob).hexdigest()+'"'
class Mock(BaseHTTPRequestHandler):
    protocol_version='HTTP/1.1'
    def log_message(self,*a):pass
    def send(self,status,body=b'',headers={}):
        self.send_response(status)
        for k,v in headers.items():self.send_header(k,v)
        self.send_header('Content-Length',str(len(body)));self.send_header('Connection','close');self.end_headers()
        try:self.wfile.write(body)
        except (BrokenPipeError,ConnectionResetError):pass
    def signature_ok(self,body=b''):
        try:
            auth=self.headers['Authorization'];parts=dict(x.split('=',1) for x in auth.removeprefix('AWS4-HMAC-SHA256 ').split(', '));access,scope=parts['Credential'].split('/',1);assert access==ACCESS
            names=parts['SignedHeaders'];canonical=''.join(n+':'+self.headers[n].strip()+'\n' for n in names.split(';'))
            payload=hashlib.sha256(body).hexdigest();assert self.headers['X-Amz-Content-Sha256']==payload
            req=self.command+'\n'+self.path+'\n\n'+canonical+'\n'+names+'\n'+payload
            sts='AWS4-HMAC-SHA256\n'+self.headers['X-Amz-Date']+'\n'+scope+'\n'+hashlib.sha256(req.encode()).hexdigest()
            key=('AWS4'+SECRET).encode()
            for part in scope.split('/'):key=hmac.new(key,part.encode(),hashlib.sha256).digest()
            return hmac.compare_digest(parts['Signature'],hmac.new(key,sts.encode(),hashlib.sha256).hexdigest())
        except Exception:return False
    def do_GET(self):
        if not self.signature_ok():self.send(403);return
        if self.path=='/bucket/cron/state':
            with lock:self.send(200,blob,{'ETag':etag}) if blob is not None else self.send(404)
        else:self.send(404)
    def do_PUT(self):
        global blob,etag,lose_put_reply
        b=self.rfile.read(int(self.headers.get('Content-Length','0')))
        if not self.signature_ok(b):self.send(403);return
        with lock:
            if fail_put:self.send(503);return
            good=self.headers.get('If-Match')==etag if blob is not None else self.headers.get('If-None-Match')=='*'
            if not good:self.send(412);return
            blob=b;etag='"'+hashlib.sha256(b).hexdigest()+'"'
            if lose_put_reply:
                lose_put_reply=False
                self.close_connection=True
                self.connection.shutdown(socket.SHUT_RDWR)
                self.connection.close()
                return
            self.send(200,b'',{'ETag':etag})
    def do_POST(self):
        b=self.rfile.read(int(self.headers.get('Content-Length','0')))
        with lock:calls.append({'path':self.path,'body':json.loads(b),'idempotency':self.headers.get('Idempotency-Key'),'user':self.headers.get('X-User'),'api_key':self.headers.get('X-Api-Key')})
        if self.path=='/chat':
            # Keep a run active briefly to prove API responsiveness and duplicate-call resistance.
            time.sleep(1)
            self.send(200,b'data: {"delta":"Scheduled answer"}\n\ndata: {"done":true}\n\n',{'Content-Type':'text/event-stream'})
        else:self.send(200,b'{"delivered":true}',{'Content-Type':'application/json'})

def port():
    s=socket.socket();s.bind(('127.0.0.1',0));p=s.getsockname()[1];s.close();return p
def stamp(offset):return datetime.fromtimestamp(time.time()+offset,timezone.utc).isoformat()
def wait_for(fn,seconds=25):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        try:
            v=fn()
            if v:return v
        except (OSError,urllib.error.URLError,ValueError):pass
        time.sleep(.1)
    raise AssertionError('Timed out waiting for condition')
def main():
    global fail_put,blob,etag,lose_put_reply
    ap=argparse.ArgumentParser();ap.add_argument('--native',action='store_true');ap.add_argument('--https-check',action='store_true');args=ap.parse_args()
    mock=ThreadingHTTPServer(('127.0.0.1',0),Mock);threading.Thread(target=mock.serve_forever,daemon=True).start();mp=mock.server_port;p=port();origin=f'http://127.0.0.1:{p}'
    config={'api_key':KEY,'storage':{'endpoint':f'http://127.0.0.1:{mp}','bucket':'bucket','key':'cron/state','region':'auto','access_key':ACCESS,'secret_key':SECRET,'master_key':MASTER},'local_test':True,'concurrency':2,'targets':{'ai':{'kind':'eyesoff','url':f'http://127.0.0.1:{mp}/chat','api_keys':{USER:'personal-synthetic-key'},'model':'test','timeout_s':30},'hook':{'kind':'http','url':f'http://127.0.0.1:{mp}/hook','users':[USER],'timeout_s':30}}}
    if args.https_check:config['targets']['tls']={'kind':'http','url':'https://eyesoff.ai/ping','method':'GET','users':[USER],'timeout_s':30}
    env={**os.environ,'ENCLAVE_CONFIG':json.dumps(config),'ENCLAVE_PORTS':f'http:8000={p}'};env.pop('ENCLAVE_EGRESS',None)
    native=ROOT/'target/debug/enclave-cron';wasm=ROOT/'target/wasm32-wasip2/release/enclave-cron.wasm'
    cmd=[str(native)] if args.native else ['wasmtime','run','-S','inherit-network=y','-S','allow-ip-name-lookup=y','--env','ENCLAVE_CONFIG','--env','ENCLAVE_PORTS',str(wasm)]
    procs=[]
    with tempfile.TemporaryDirectory(prefix='enclave-cron-test-') as td:
        log=open(Path(td)/'service.log','w+')
        def launch():
            pr=subprocess.Popen(cmd,env=env,stdout=log,stderr=log);procs.append(pr);return pr
        def req(path,body=None,user=USER,key=KEY):
            h={'X-Api-Key':key,'X-User':user,'Content-Type':'application/json'}
            r=urllib.request.Request(origin+path,data=None if body is None else json.dumps(body).encode(),headers=h)
            try:
                with urllib.request.urlopen(r,timeout=5) as f:return f.status,json.load(f)
            except urllib.error.HTTPError as e:return e.code,json.load(e)
        def call(name,body={},user=USER):
            code,v=req('/api/'+name,body,user);assert code==200,(code,v);return v
        def spec(key,at=5):return {'name':key,'client_key':key,'schedule':{'kind':'once','at':stamp(at)},'action':{'kind':'eyesoff','target':'ai','prompt':'synthetic scheduled prompt'}}
        try:
            pr=launch();wait_for(lambda:req('/ping')[0]==200)
            assert blob.startswith(b'CRN1') and MASTER.encode() not in blob
            assert req('/api/schedule_list',{},key='wrong')[0]==401
            assert call('schedule_targets',user=OTHER)['targets']==[]
            assert req('/api/schedule_create',spec('unauthorized'),user=OTHER)[0]==400
            val=spec('once');j=call('schedule_create',val)['job'];assert call('schedule_create',val)['job']['id']==j['id']
            assert b'synthetic scheduled prompt' not in blob
            assert call('schedule_list',user=OTHER)['jobs']==[]
            assert req('/api/schedule_get',{'id':j['id']},user=OTHER)[0]==400
            wait_for(lambda:any(c['path']=='/chat' for c in calls))
            run=wait_for(lambda:next((r for r in call('schedule_get',{'id':j['id']})['runs'] if r['status']=='succeeded'),None))
            assert run['result']=='Scheduled answer' and len(calls)==1
            assert calls[0]['user']==USER and calls[0]['api_key']=='personal-synthetic-key'
            code,m=req('/mcp',{'jsonrpc':'2.0','id':1,'method':'tools/list'});assert code==200 and len(m['result']['tools'])==7
            # A duplicate explicit run-now in the same second dispatches once.
            a=call('schedule_run_now',{'id':j['id'],'request_key':'same'})['run'];b=call('schedule_run_now',{'id':j['id'],'request_key':'same'})['run'];assert a['id']==b['id']
            wait_for(lambda:len(calls)==2);wait_for(lambda:any(r['id']==a['id'] and r['status']=='succeeded' for r in call('schedule_get',{'id':j['id']})['runs']))
            hook=spec('hook-test',3600);hook['action']={'kind':'http','target':'hook','body':{'synthetic':'webhook'}};hj=call('schedule_create',hook)['job'];hr=call('schedule_run_now',{'id':hj['id'],'request_key':'hook-once'})['run'];wait_for(lambda:any(r['id']==hr['id'] and r['status']=='succeeded' for r in call('schedule_get',{'id':hj['id']})['runs']));assert calls[-1]['body']=={'synthetic':'webhook'} and len(calls)==3
            # Check authenticated MCP call compatibility, not only discovery.
            code,m=req('/mcp',{'jsonrpc':'2.0','id':2,'method':'tools/call','params':{'name':'schedule_list','arguments':{}}});assert code==200 and not m['result']['isError']
            if args.https_check:
                tls=spec('tls-check',3600);tls['action']={'kind':'http','target':'tls','body':{}};tj=call('schedule_create',tls)['job'];tr=call('schedule_run_now',{'id':tj['id'],'request_key':'tls'})['run'];wait_for(lambda:any(r['id']==tr['id'] and r['status']=='succeeded' for r in call('schedule_get',{'id':tj['id']})['runs']),40)
            # Another process cannot bind the same listener.
            second=launch();assert second.wait(timeout=15)!=0
            # Crash before a due time, skip the missed one-time occurrence after recovery.
            missed=call('schedule_create',spec('missed',2))['job'];pr.kill();pr.wait();time.sleep(3);expire_lease();pr=launch();wait_for(lambda:req('/ping')[0]==200)
            saved=call('schedule_get',{'id':missed['id']});assert saved['runs'][0]['status']=='skipped' and not saved['job']['enabled'];assert len(calls)==3
            # Durable state is present after restart; isolated user still cannot read it.
            assert call('schedule_get',{'id':j['id']})['runs'][0]['status']=='succeeded'
            # An in-flight callback is never automatically replayed after a crash.
            ir=call('schedule_run_now',{'id':j['id'],'request_key':'interrupt'})['run'];wait_for(lambda:len(calls)==4);pr.kill();pr.wait();expire_lease();pr=launch();wait_for(lambda:req('/ping')[0]==200)
            assert next(r for r in call('schedule_get',{'id':j['id']})['runs'] if r['id']==ir['id'])['status']=='interrupted';assert len(calls)==4
            # A competing writer fences the current ownership session but keeps
            # the SAME process/listener alive. Nothing is sent until CAS reacquisition.
            with lock:etag='"changed-by-competing-writer"'
            assert req('/api/schedule_run_now',{'id':j['id'],'request_key':'conflict-must-not-send'})[0]==503
            assert pr.poll() is None and len(calls)==4
            assert req('/ping')[0]==503 and req('/api/schedule_list',{})[0]==503
            time.sleep(2);assert req('/ping')[0]==503 and len(calls)==4
            expire_lease();wait_for(lambda:req('/ping')[0]==200,40)
            assert pr.poll() is None and len(calls)==4
            assert not any(r['id']==hashlib.sha256((j['id']+':manual:conflict-must-not-send').encode()).hexdigest() for r in opened()['runs'])
            # A transient storage failure does not acknowledge or dispatch, destroy
            # the serving process, or require a new certificate after recovery.
            fail_put=True
            assert req('/api/schedule_run_now',{'id':j['id'],'request_key':'must-not-send'})[0]==503
            assert pr.poll() is None and len(calls)==4 and req('/ping')[0]==503
            fail_put=False;expire_lease();wait_for(lambda:req('/ping')[0]==200,40)
            assert pr.poll() is None and len(calls)==4
            # Idle lease renewal takes the same safe recovery path, without any
            # API mutation being necessary to trigger a storage failure.
            fail_put=True
            wait_for(lambda:req('/ping')[0]==503,40)
            assert pr.poll() is None and len(calls)==4
            fail_put=False;expire_lease();wait_for(lambda:req('/ping')[0]==200,40)
            assert pr.poll() is None and len(calls)==4
            # The storage accepted a run claim but the reply was lost. Recover from
            # durable state without replaying an effect whose outcome is unknown.
            lose_put_reply=True
            assert req('/api/schedule_run_now',{'id':j['id'],'request_key':'lost-reply'})[0]==503
            assert pr.poll() is None and len(calls)==4
            claimed=opened()['runs'][-1]['id']
            expire_lease();wait_for(lambda:req('/ping')[0]==200,40)
            recovered=next(r for r in call('schedule_get',{'id':j['id']})['runs'] if r['id']==claimed)
            assert recovered['status']=='interrupted' and len(calls)==4
            assert call('schedule_run_now',{'id':j['id'],'request_key':'lost-reply'})['run']['id']==claimed
            assert len(calls)==4
            # A recovered ownership session can dispatch new work exactly once.
            rr=call('schedule_run_now',{'id':j['id'],'request_key':'after-recovery'})['run']
            wait_for(lambda:any(r['id']==rr['id'] and r['status']=='succeeded' for r in call('schedule_get',{'id':j['id']})['runs']))
            assert len(calls)==5
            # Corrupt durable state stays unavailable and is never replaced.
            pr.kill();pr.wait()
            with lock:blob=blob[:-1]+bytes([blob[-1]^1]);corrupted=blob
            corrupt=launch();wait_for(lambda:req('/ping')[0]==503)
            time.sleep(2)
            assert corrupt.poll() is None and blob==corrupted and len(calls)==5
            print(json.dumps({'mode':'native' if args.native else 'wasm32-wasip2','passed':['SigV4 storage signing','encrypted durable writes','HTTP webhook dispatch','CAS lease exclusivity','per-user API/target isolation','autonomous one-time dispatch','authenticated Eyesoff callback','MCP tools','same-second idempotent manual run','restart preserves jobs/results','skip missed after crash','storage failure fences effects','interrupted delivery is not replayed','live CAS conflict fences effects','corrupt state fails closed','storage recovery preserves process/listener','ambiguous accepted write never replays','new work succeeds after reacquisition'],'callbacks':len(calls)}))
        finally:
            for pr in procs:
                if pr.poll() is None:pr.kill();pr.wait()
            log.flush();log.seek(0)
            if sys.exc_info()[0] is not None:print(log.read())
            mock.shutdown()
if __name__=='__main__':main()
