#!/usr/bin/env python3
"""Run one isolated real-ICD test; a worker is needed only for masked cases."""
from pathlib import Path
import argparse,json,subprocess,sys
p=argparse.ArgumentParser();p.add_argument('--image',required=True);p.add_argument('--mesa',type=Path,required=True);p.add_argument('--sdk',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--worker-port',type=int,default=9741);p.add_argument('--groups',type=int,default=1);p.add_argument('--cpu-only',action='store_true');p.add_argument('--corrupt-at',type=int,default=1);p.add_argument('--memory-mib',type=int,default=2048);p.add_argument('mode',choices=['gl-queries','gl-wide-range','gl-unavailable','gl-descriptor-buffer','gl-masked','gl-local','gl-worker-corrupt','equivalent','masked','local','bad-reply','wrong-peer','unavailable','worker-corrupt','unmatched','dispatch-base','specialization','dynamic-bindings','wide-range','queries','generated','bad-reply-timeline','bad-reply-timeline-before','bad-reply-queue-idle','bad-reply-device-idle']);a=p.parse_args()
assert 1<=a.groups<=65 and 1<=a.corrupt_at<=a.groups and 192<=a.memory_mib<=2048
assert a.image.startswith('sha256:') and len(a.image)==71 and 0<a.worker_port<65536
source=Path(__file__).resolve().parent;compute=source.parent.parent/'shield-compute';output=a.output.resolve();mesa=a.mesa.resolve();sdk=a.sdk.resolve()
for required in [output/'build/prepass',output/'triangle.rgba',mesa/'lib/libvulkan_lvp.so',sdk/'lib/libshield-core.so']:assert required.is_file(),required
log=output/('driver-'+a.mode+'.log');assert not log.exists() and not (output/('driver-'+a.mode+'.rgba')).exists(),'preserve earlier evidence in a separate output directory'
# Unique container name and exact finally-cleanup; never remove unrelated work.
import uuid
name='shield-graphics-test-'+uuid.uuid4().hex[:12]
args=['docker','run','--rm','--init','--name',name,'--network','host' if a.mode in ['masked','worker-corrupt','equivalent','gl-masked','gl-worker-corrupt'] else 'none','--read-only','--cpus','2','--memory',str(a.memory_mib)+'m','--memory-swap',str(a.memory_mib)+'m','--tmpfs','/tmp:rw,mode=1777','--tmpfs','/run:rw,mode=755','-e','SHIELD_TEST_GROUPS='+str(a.groups),'-e','SHIELD_TEST_CPU_ONLY='+str(int(a.cpu_only)),'-e','SHIELD_TEST_CORRUPT_AT='+str(a.corrupt_at),'-e','SHIELD_TEST_WORKER_PORT='+str(a.worker_port),'-v',str(compute)+':/compute:ro','-v',str(sdk/'lib')+':/opt/shield/lib:ro','-v',str(mesa)+':/opt/shield/mesa:ro','-v',str(output)+':/output:rw','-v',str(source/'test-driver.py')+':/test-driver.py:ro',a.image,'python3','/test-driver.py',a.mode]
try:
 with log.open('x') as f:result=subprocess.run(args,stdout=f,stderr=subprocess.STDOUT,timeout=60)
 print(json.dumps({'mode':a.mode,'exitCode':result.returncode,'log':str(log)}))
finally:subprocess.run(['docker','rm','-f',name],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
sys.exit(result.returncode)
