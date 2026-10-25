#!/usr/bin/env python3
"""Bounded public-image comparisons using only the installed candidate runtime.

Uses the isolated host test worker 9741; this is numerical evidence, not CVM or
quality acceptance. No development runtime library is mounted into the image.
"""
import base64
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time
sys.path.insert(0, '/shield')
from service import send, receive
from evaluate_numerics import IMAGES


def identity(uid):
    def drop():
        os.setgroups([]); os.setgid(1000); os.setuid(uid)
    return drop


def main():
    assert os.getuid() == 0
    out = Path('/evidence')
    config = json.loads(Path('/shield/service.json').read_text())
    assert config['enableQ8Experimental'] is True
    config['worker'] = {'host': '127.0.0.1', 'port': 9741}
    path = Path('/tmp/numerics-service.json')
    path.write_text(json.dumps(config))
    for name, uid, gid, mode in [('/run/shield',1001,1000,0o750),
                                ('/run/shield/jobs',1001,1001,0o700),
                                ('/tmp/desktop',1000,1000,0o700)]:
        Path(name).mkdir(parents=True, exist_ok=True)
        os.chown(name,uid,gid); os.chmod(name,mode)
    def request(value):
        with socket.socket(socket.AF_UNIX) as client:
            client.settimeout(300); client.connect('/run/shield/compute.sock')
            send(client,value)
            result = receive(client,8<<20)
            assert result.get('ok'),result
            return result
    def status(): return request({'op':'status'})
    receipt = {'schema':'shield-standard-image-numerics/1',
        'scope':'public diagnostic fixtures in isolated host container; descriptive metrics, not general quality acceptance',
        'modelPolicies':{k:v['policySha256'] for k,v in config['models'].items()},'images':[]}
    def save(): (out/'numerics.json').write_text(json.dumps(receipt,indent=2)+'\n')
    service_log = (out/'service.log').open('wb')
    service = subprocess.Popen(['/usr/local/bin/shield-sandbox','/usr/bin/python3','-I','/shield/service.py','--config',str(path)],
        preexec_fn=identity(1001), stdout=service_log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        for _ in range(200):
            if service.poll() is not None: raise RuntimeError('service exited')
            if Path('/run/shield/compute.sock').exists():
                try:
                    if status()['state']=='ready': break
                except OSError: pass
            time.sleep(.1)
        else: raise RuntimeError('service readiness timeout')
        for name,prompt,seed in IMAGES:
            record={'name':name,'prompt':prompt,'seed':seed,'steps':8,'runs':{}}
            receipt['images'].append(record); save()
            for mode in ('cpu','masked','standard'):
                start=time.monotonic()
                if mode != 'standard':
                    result=request({'op':'sd','mode':mode,'prompt':prompt,'seed':seed,
                        'arithmetic':'shield-fixed-point-v1' if mode=='masked' else 'native-model-v1'})
                    png=base64.b64decode(result['png'],validate=True)
                    stats=result['stats']
                    if mode=='masked': assert stats['exchanges']>0 and stats['verification_failures']==0
                else:
                    before=status()['telemetry']
                    dest=Path('/tmp/desktop/result.png'); dest.unlink(missing_ok=True)
                    command=['/usr/local/bin/shield-sandbox','/usr/local/bin/sd-cli','-m',config['models']['sd']['model'],
                        '-p',prompt,'-o',str(dest),'--width','256','--height','256','--steps','8','--cfg-scale','7',
                        '--sampling-method','euler','--seed',str(seed),'--threads','2','--backend','GPUOpenCL','--mmap','--diffusion-fa']
                    env={'PATH':'/usr/local/bin:/usr/bin:/bin','HOME':'/tmp/desktop','LANG':'C.UTF-8',
                        'POCL_CACHE_DIR':'/tmp/desktop/pocl-cache','SHIELD_OPENCL_ARITHMETIC':'shield-fixed-point-v1'}
                    with (out/(name+'.stdout')).open('wb') as stdout,(out/(name+'.stderr')).open('wb') as stderr:
                        process=subprocess.Popen(command,env=env,cwd='/tmp/desktop',preexec_fn=identity(1000),
                            stdout=stdout,stderr=stderr,start_new_session=True)
                        try: process.wait(timeout=300)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid,signal.SIGKILL);process.wait();raise
                    after=status()
                    assert process.returncode==0 and after['state']=='ready',(process.returncode,after)
                    stats={'exchanges':after['telemetry']['verifiedOffloads']-before['verifiedOffloads'],
                        'macs':after['telemetry']['maskedMacs']-before['maskedMacs']}
                    assert stats['exchanges']>0
                    png=dest.read_bytes()
                (out/(name+'-'+mode+'.png')).write_bytes(png)
                record['runs'][mode]={'seconds':time.monotonic()-start,'stats':stats,
                    'pngSha256':hashlib.sha256(png).hexdigest()}
                save(); print(name+' '+mode+': '+json.dumps(record['runs'][mode]),flush=True)
            runs=record['runs']
            record['standardMatchesNativeMaskedPng']=runs['standard']['pngSha256']==runs['masked']['pngSha256']
            record['sameVerifiedWork']=all(runs['standard']['stats'][k]==runs['masked']['stats'][k] for k in ('exchanges','macs'))
            save()
        receipt['generationComplete']=True
        receipt['allStandardMatchesNativeMasked']=all(x['standardMatchesNativeMaskedPng'] and x['sameVerifiedWork'] for x in receipt['images'])
        save()
        assert receipt['allStandardMatchesNativeMasked'], 'standard/native mismatch; evidence retained'
    finally:
        save()
        os.killpg(service.pid,signal.SIGTERM)
        try: service.wait(timeout=15)
        except subprocess.TimeoutExpired: os.killpg(service.pid,signal.SIGKILL);service.wait()
        service_log.close()


if __name__=='__main__': main()
