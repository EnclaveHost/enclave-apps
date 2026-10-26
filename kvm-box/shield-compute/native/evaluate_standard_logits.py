#!/usr/bin/env python3
"""Public teacher-forced comparisons, installed runtime, test-only evaluator.

Run in a separate bounded host container. This test alone permits 128 MiB
public diagnostic files; production service limits are unchanged. Masked jobs
are serial and use only the dedicated loopback worker 9741.
"""
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys
import threading
import time
sys.path.insert(0,'/shield')
import service
from evaluate_numerics import TEXTS,compare_logits


def main():
    out=Path('/evidence')
    resource.setrlimit(resource.RLIMIT_CORE,(0,0))
    resource.setrlimit(resource.RLIMIT_FSIZE,(128<<20,128<<20))
    config=json.loads(Path('/shield/service.json').read_text())
    config['worker']={'host':'127.0.0.1','port':9741}
    config['jobs']='/tmp';config['clientUids']=[os.getuid()]
    assert config['enableQ8Experimental'] is True
    compute=service.Compute(config)
    Path(config['socket']).parent.mkdir(parents=True,exist_ok=True)
    server=service.Server(config['socket'],service.Handler);server.compute=compute
    thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
    receipt={'schema':'shield-standard-logit-numerics/1',
        'scope':'public diagnostic fixtures in host container; no general model-quality acceptance',
        'settings':{'batch':32,'threads':2,'flashAttention':False,'kvOffload':False,'extraCpuBufferTypes':False},
        'evaluatorSha256':hashlib.sha256((out/'evaluate-standard-logits').read_bytes()).hexdigest(),
        'modelSha256':config['models']['llama']['modelSha256'],
        'policySha256':config['models']['llama']['policySha256'],'texts':[]}
    def save(): (out/'numerics.json').write_text(json.dumps(receipt,indent=2)+'\n')
    try:
        for name,text in TEXTS.items():
            prompt=out/(name+'.txt');prompt.write_text(text)
            record={'name':name,'text':text,'runs':{}};receipt['texts'].append(record)
            for mode in ('cpu','masked','standard'):
                # Each evaluator process owns its SDK reservation; release the
                # previous standard client's persistent broker before changing
                # owners. This mirrors Compute.run's native-job transition.
                with compute.lock:
                    if compute.q8_broker is not None: compute.q8_broker.close()
                env=compute.environment(config['models']['llama'],mode=='masked','/tmp','llama')
                if mode=='standard':
                    env.update(GGML_SHIELD_NATIVE_CPU_FALLBACK='1',SHIELD_OPENCL_ARITHMETIC='shield-fixed-point-v1',
                        POCL_MAX_PTHREAD_COUNT='2',POCL_CACHE_DIR='/tmp/pocl-cache')
                path=out/(name+'-'+mode+'.logits');before=dict(compute.telemetry);start=time.monotonic()
                command=['/evidence/evaluate-standard-logits',config['models']['llama']['model'],str(prompt),str(path),
                    'opencl' if mode=='standard' else 'cpu']
                with (out/(name+'-'+mode+'.stdout')).open('wb') as stdout,(out/(name+'-'+mode+'.stderr')).open('wb') as stderr:
                    process=subprocess.run(command,env=env,stdout=stdout,stderr=stderr,timeout=600)
                assert process.returncode==0,(name,mode,process.returncode)
                stats=service.parse_native_stats((out/(name+'-'+mode+'.stderr')).read_text())
                if mode=='standard':
                    assert not compute.retired
                    stats={'exchanges':compute.telemetry['verifiedOffloads']-before['verifiedOffloads'],
                        'macs':compute.telemetry['maskedMacs']-before['maskedMacs']}
                if mode!='cpu': assert stats and stats['exchanges']>0 and stats.get('verification_failures',0)==0,stats
                record['runs'][mode]={'seconds':time.monotonic()-start,'stats':stats,
                    'logitsSha256':hashlib.sha256(path.read_bytes()).hexdigest()}
                save();print(name+' '+mode+': '+json.dumps(record['runs'][mode]),flush=True)
            record['cpuVsStandard']=compare_logits(out/(name+'-cpu.logits'),out/(name+'-standard.logits'))
            record['cpuVsNativeMasked']=compare_logits(out/(name+'-cpu.logits'),out/(name+'-masked.logits'))
            record['nativeMaskedVsStandard']=compare_logits(out/(name+'-masked.logits'),out/(name+'-standard.logits'))
            record['standardMatchesNativeMaskedLogits']=record['runs']['standard']['logitsSha256']==record['runs']['masked']['logitsSha256']
            save();print(name+' comparison: '+json.dumps(record['cpuVsStandard']),flush=True)
            for mode in ('cpu','masked','standard'): (out/(name+'-'+mode+'.logits')).unlink()
        receipt['complete']=True;save()
    finally:
        server.shutdown();server.server_close();thread.join(timeout=5)
        with compute.lock:
            if compute.q8_broker is not None: compute.q8_broker.close()
        save()


if __name__=='__main__': main()
