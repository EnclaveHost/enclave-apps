#!/usr/bin/env python3
"""Diagnostic baseline vs explicit attention placement; no production changes.

Uses the installed candidate, public Qwen fixture, no CPU repacking, identical
arithmetic, and the dedicated test GPU. Scheduler split logs are retained to
check the repeated-weight-copy hypothesis before changing the loader.
"""
import hashlib,json,os
from pathlib import Path
import subprocess,sys,threading,time
sys.path.insert(0,'/shield')
import service

out=Path('/evidence');config=json.loads(Path('/shield/service.json').read_text())
config['worker']={'host':'127.0.0.1','port':9741};config['jobs']='/tmp';config['clientUids']=[0]
compute=service.Compute(config);Path(config['socket']).parent.mkdir(parents=True,exist_ok=True)
server=service.Server(config['socket'],service.Handler);server.compute=compute
thread=threading.Thread(target=server.serve_forever,daemon=True);thread.start()
receipt={'schema':'shield-weight-placement-diagnostic/1','scope':'host diagnostic, not statistically reliable performance benchmark','runs':[]}
try:
    for label,override in [('warmup',False),('baseline',False),('attention-cpu',True)]:
        command=['/usr/local/bin/llama-cli','-m',config['models']['llama']['model'],'--device','GPUOpenCL','-ngl','99',
            '-p','The capital of France is','-n','8','-c','512','-b','32','-ub','32','-t','2','--temp','0','--seed','42',
            '--flash-attn','off','--verbosity','5','--no-conversation','--single-turn','--no-warmup','--no-display-prompt','--color','off',
            '--no-repack','--fit','off','--no-mmap']
        if override: command+=['--override-tensor',r'blk\.[0-9]+\.attn_.*=CPU']
        env={'PATH':'/usr/local/bin:/usr/bin:/bin','HOME':'/tmp','LANG':'C.UTF-8','POCL_CACHE_DIR':'/tmp/pocl-cache',
            'SHIELD_OPENCL_ARITHMETIC':'shield-fixed-point-v1','GGML_SCHED_DEBUG':'1'}
        # Reset the native verified-result group cache between processes;
        # retain only PoCL's CPU program cache for comparable warm starts.
        if compute.q8_broker is not None: compute.q8_broker.close()
        before=dict(compute.telemetry);start=time.monotonic()
        with (out/(label+'.stdout')).open('wb') as stdout,(out/(label+'.stderr')).open('wb') as stderr:
            p=subprocess.run(command,env=env,stdout=stdout,stderr=stderr,timeout=300)
        assert p.returncode==0 and not compute.retired,(label,p.returncode)
        record={'label':label,'command':command,'seconds':time.monotonic()-start,
            'verifiedExchanges':compute.telemetry['verifiedOffloads']-before['verifiedOffloads'],
            'maskedMacs':compute.telemetry['maskedMacs']-before['maskedMacs'],
            'stdoutSha256':hashlib.sha256((out/(label+'.stdout')).read_bytes()).hexdigest()}
        assert record['verifiedExchanges']>0
        receipt['runs'].append(record);(out/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
        print(json.dumps(record),flush=True)
    a,b=receipt['runs'][-2:];receipt['sameVerifiedWork']=all(a[k]==b[k] for k in ('verifiedExchanges','maskedMacs'))
    assert receipt['sameVerifiedWork'], 'placement changed verified work'
    receipt['complete']=True
finally:
    server.shutdown();server.server_close();thread.join(timeout=5)
    if compute.q8_broker is not None:compute.q8_broker.close()
    (out/'receipt.json').write_text(json.dumps(receipt,indent=2)+'\n')
