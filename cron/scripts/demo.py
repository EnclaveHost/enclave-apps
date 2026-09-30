#!/usr/bin/env python3
"""Local-only preview with synthetic credentials, storage and callbacks. Never production."""
import importlib.util,json,os,signal,subprocess,sys,tempfile,threading,time
from pathlib import Path
spec=importlib.util.spec_from_file_location('cron_e2e',Path(__file__).resolve().parents[1]/'tests/e2e.py')
m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
mock=m.ThreadingHTTPServer(('127.0.0.1',0),m.Mock);threading.Thread(target=mock.serve_forever,daemon=True).start()
p=m.port();mp=mock.server_port
config={'api_key':m.KEY,'local_test':True,'storage':{'endpoint':f'http://127.0.0.1:{mp}','bucket':'bucket','key':'cron/state','region':'auto','access_key':m.ACCESS,'secret_key':m.SECRET,'master_key':m.MASTER},'targets':{'eyesoff-demo':{'kind':'eyesoff','url':f'http://127.0.0.1:{mp}/chat','api_keys':{m.USER:'synthetic-personal-key'},'timeout_s':30}}}
env={**os.environ,'ENCLAVE_CONFIG':json.dumps(config),'ENCLAVE_PORTS':f'http:8000={p}'};env.pop('ENCLAVE_EGRESS',None)
cmd=['wasmtime','run','-S','inherit-network=y','-S','allow-ip-name-lookup=y','--env','ENCLAVE_CONFIG','--env','ENCLAVE_PORTS',str(m.ROOT/'target/wasm32-wasip2/release/enclave-cron.wasm')]
proc=subprocess.Popen(cmd,env=env)
print(json.dumps({'url':f'http://127.0.0.1:{p}','synthetic_key':m.KEY,'synthetic_user':m.USER}),flush=True)
def end(*_):
    proc.terminate()
    try:proc.wait(timeout=5)
    except subprocess.TimeoutExpired:proc.kill();proc.wait()
    mock.shutdown();sys.exit(0)
signal.signal(signal.SIGTERM,end);signal.signal(signal.SIGINT,end)
try:proc.wait()
finally:
    if proc.poll() is None:proc.terminate()
    mock.shutdown()
