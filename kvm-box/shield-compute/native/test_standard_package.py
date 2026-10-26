#!/usr/bin/env python3
"""Container integration: installed standard apps, real uid split, masked service.

Run as container root, with public /models, writable /evidence and the dedicated
loopback worker. This checks seccomp and uid/socket ownership, not VM nftables
or SNP attestation. It never mounts development runtime libraries.
"""
import ctypes
import glob
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import sys
import time
sys.path.insert(0, '/shield')
from service import send, receive

assert os.getuid() == 0
out = Path('/evidence')
config = json.loads(Path('/shield/service.json').read_text())
assert config['enableQ8Experimental'] is True
config['worker'] = {'host': '127.0.0.1', 'port': 9741}
fault = sys.argv[1] if len(sys.argv)>1 else None
assert fault in (None, 'corrupt', 'disconnect')
proxy = None
if fault:
    sys.path.insert(0, '/tests')
    from test_wide import Proxy
    proxy = Proxy(1, fault_kind=fault)
    config['worker']['port'] = proxy.port
config_path = Path('/tmp/standard-service.json')
config_path.write_text(json.dumps(config))
for name, uid, gid, mode in [('/run/shield',1001,1000,0o750),
                             ('/run/shield/jobs',1001,1001,0o700),
                             ('/tmp/desktop',1000,1000,0o700)]:
    Path(name).mkdir(parents=True,exist_ok=True);os.chown(name,uid,gid);os.chmod(name,mode)

def identity(uid):
    def drop():
        os.setgroups([]);os.setgid(1000);os.setuid(uid)
    return drop

def status():
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(10);client.connect('/run/shield/compute.sock');send(client,{'op':'status'})
        return receive(client,65536)

service_log=(out/'service.log').open('wb')
service=subprocess.Popen(['/usr/local/bin/shield-sandbox','/usr/bin/python3','-I','/shield/service.py','--config',str(config_path)],
    preexec_fn=identity(1001),stdout=service_log,stderr=subprocess.STDOUT,start_new_session=True)
results={}
try:
    for _ in range(200):
        if service.poll() is not None:raise RuntimeError('service exited before readiness')
        if Path('/run/shield/compute.sock').exists():
            try:
                if status()['state']=='ready':break
            except OSError:pass
        time.sleep(.1)
    else:raise RuntimeError('service readiness timeout')
    env={'PATH':'/usr/local/bin:/usr/bin:/bin','HOME':'/tmp/desktop','LANG':'C.UTF-8',
         'POCL_CACHE_DIR':'/tmp/desktop/pocl-cache','SHIELD_OPENCL_ARITHMETIC':'shield-fixed-point-v1'}
    sandbox=['/usr/local/bin/shield-sandbox']
    check='''import ctypes,errno,glob,json,os,socket
assert os.getuid()==1000
assert ctypes.CDLL(None).prctl(39,0,0,0,0)==1
try: socket.socket(socket.AF_VSOCK,socket.SOCK_STREAM)
except PermissionError: pass
else: raise AssertionError('VSOCK allowed')
devices=glob.glob('/dev/nvidia*')+glob.glob('/dev/dri/*')+glob.glob('/dev/kfd');assert not devices
print(json.dumps({'uid':os.getuid(),'rawGpuDevices':devices,'vsockDenied':True,'noNewPrivs':True}))
'''
    boundary=subprocess.run(sandbox+['/usr/bin/python3','-c',check],env=env,preexec_fn=identity(1000),capture_output=True,text=True,check=True)
    results['boundary']=json.loads(boundary.stdout)
    cases={
      'sd':['/usr/local/bin/sd-cli','-m',config['models']['sd']['model'],'-p','A watercolor cabin beside a lake, pine trees, morning light',
        '-o','/tmp/desktop/result.png','--width','256','--height','256','--steps','2','--cfg-scale','7','--sampling-method','euler',
        '--seed','42','--threads','2','--backend','GPUOpenCL','--mmap','--diffusion-fa'],
      'llama':['/usr/local/bin/llama-cli','-m',config['models']['llama']['model'],'--device','GPUOpenCL','-ngl','99',
        '-p','The capital of France is','-n','8','-c','512','-b','32','-ub','32','-t','2','--temp','0','--seed','42',
        '--flash-attn','off','--verbosity','4','--no-conversation','--single-turn','--no-warmup','--no-display-prompt','--color','off']}
    for name,command in cases.items():
        if fault and name != 'sd':continue
        before=status()['telemetry'];start=time.monotonic()
        with (out/(name+'.stdout')).open('wb') as stdout,(out/(name+'.stderr')).open('wb') as stderr:
            process=subprocess.Popen(sandbox+command,env=env,cwd='/tmp/desktop',preexec_fn=identity(1000),stdout=stdout,stderr=stderr,start_new_session=True)
            try:process.wait(timeout=300)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid,signal.SIGKILL);process.wait();raise
        after=status();result={'exitCode':process.returncode,'seconds':time.monotonic()-start,'command':command,
            'verifiedExchanges':after['telemetry']['verifiedOffloads']-before['verifiedOffloads'],
            'maskedMacs':after['telemetry']['maskedMacs']-before['maskedMacs'],'serviceState':after['state']}
        results[name]=result
        if fault:
            proxy.thread.join(timeout=20)
            assert process.returncode!=0 and after['state']=='retired'
            assert not Path('/tmp/desktop/result.png').exists()
            assert result['verifiedExchanges']==0 and result['maskedMacs']==0
            assert not proxy.thread.is_alive() and proxy.error is None
            assert (proxy.modified==1 if fault=='corrupt' else proxy.dropped==1)
            with socket.socket(socket.AF_UNIX) as client:
                client.settimeout(10);client.connect('/run/shield/compute.sock')
                send(client,{'op':'q8_0_matmul'});retry=receive(client,65536)
            assert not retry.get('ok') and 'retired' in retry.get('error',''),retry
            results['fault']={'kind':fault,'modified':proxy.modified,'dropped':proxy.dropped,
                              'retryRefused':True,'noPublishedImage':True}
            results['passed']=True
            print(fault+' fail-closed check passed',flush=True)
            break
        assert process.returncode==0 and after['state']=='ready' and result['verifiedExchanges']>0,result
        if name=='sd':
            shutil.copyfile('/tmp/desktop/result.png',out/'sd.png')
            result['pngSha256']=hashlib.sha256((out/'sd.png').read_bytes()).hexdigest()
            assert result['pngSha256']=='048b2445a2422e1fca0c8c8272557e6ab829dbcc74cfa818edf8e0c61d7abf80',result
            assert result['verifiedExchanges']==392 and result['maskedMacs']==51921289216
        else:
            assert 'Paris' in (out/'llama.stdout').read_text()
            assert result['verifiedExchanges']==511 and result['maskedMacs']==14478327808
            assert 'listening on http://127.0.0.1:' in (out/'llama.stderr').read_text()
        print(name+' passed',flush=True)
    # The native service path must retain its private loopback identity even
    # after ordinary desktop apps have loaded the standard OpenCL backend.
    if fault:
        raise SystemExit(0)
    command=['/opt/shield/bin/llama-cli','-m',config['models']['llama']['model'],
        '--device','none','-ngl','0','-p','The capital of France is','-n','8',
        '-c','512','-t','2','--temp','0','--no-conversation','--single-turn',
        '--no-warmup','--no-display-prompt','--color','off','--verbosity','4']
    service_env={'PATH':'/usr/bin:/bin','HOME':'/run/shield/jobs','LANG':'C.UTF-8',
                 'GGML_BACKEND_PATH':'/opt/shield/lib/backends/libggml-cpu.so'}
    with (out/'private-cli.stdout').open('wb') as stdout,(out/'private-cli.stderr').open('wb') as stderr:
        private=subprocess.run(sandbox+command,env=service_env,cwd='/run/shield/jobs',
            preexec_fn=identity(1001),stdout=stdout,stderr=stderr,timeout=60)
    assert private.returncode==0
    assert 'Paris' in (out/'private-cli.stdout').read_text()
    assert 'listening on http://127.0.0.3:' in (out/'private-cli.stderr').read_text()
    with socket.socket(socket.AF_UNIX) as client:
        client.settimeout(120);client.connect('/run/shield/compute.sock')
        send(client,{'op':'llama','mode':'masked','arithmetic':'shield-fixed-point-v1',
                     'prompt':'The capital of France is','tokens':8,'seed':42})
        native=receive(client,8<<20)
    assert native.get('ok') and native['stats']['exchanges']>0 and native['stats']['verification_failures']==0,native
    results['nativeService']={'privateLoopbackVerified':True,'stats':native['stats']}
    assert not (out/'q8-capture.jsonl').exists(), 'packaged frontend wrote development audit log'
    results['passed']=True
finally:
    results.update(schema='shield-standard-package-test/1',scope='host container, installed files, uid split and seccomp; not CVM/firewall proof')
    (out/'receipt.json').write_text(json.dumps(results,indent=2)+'\n')
    os.killpg(service.pid,signal.SIGTERM)
    try:service.wait(timeout=15)
    except subprocess.TimeoutExpired:os.killpg(service.pid,signal.SIGKILL);service.wait()
    service_log.close()
