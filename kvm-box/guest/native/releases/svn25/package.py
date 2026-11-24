"""Offline packaging of the Shield-route candidate (intended SVN25) up to the unsigned IGVM; no signing, deployment or
admission here. Follows native-audio-image-v21/package.py. The platform source is SVN23's and SVN24's (branch
kvm-box/purchased-shares-20261120 at 3ee89bc16); only the root image named in the profile changes."""
from pathlib import Path
import hashlib,json,os,shutil,subprocess,time
h=Path(__file__).resolve().parent;w=h.parent;old=w/'native-audio-image-v21';tc=w/'native-terminal-image-v15';flow=w/'native-flow-image-v18'
def read(p):return json.loads(p.read_text())
def sha(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def write(p,v):
 with p.open('x') as f:json.dump(v,f,indent=2);f.write('\n')
assert read(h/'probe/valid.result.json')['passed']
meta=read(h/'root-image/image.json');assert meta['userspaceVerityVerified'] and meta['baseUnchanged'] and all(meta['corruptionRefused'].values())
before=read(old/'profile.json');p=json.loads(json.dumps(before));r,v=meta['image'],meta['verity']
p['root']={'dataBlocks':v['dataBlocks'],'hashBytes':v['bytes'],'hashSha256':v['sha256'],'imageBytes':r['bytes'],'imageSha256':r['sha256'],'rootHash':v['rootHash'],'salt':v['salt']}
# The controller is SVN20's, byte for byte: the page and the catalog version do not change.
assert meta['sourcePins']['controller']==before['controllerSha256']
assert sorted(k for k in p if p[k]!=before[k])==['root'] and p.keys()==before.keys()
with (h/'profile.json').open('xb') as f:f.write((json.dumps(p,sort_keys=True,separators=(',',':'))+'\n').encode())
prior=read(old/'spec.json');spec=json.loads(json.dumps(prior));assert spec['profile']['path']==str(old/'profile.json')
spec['profile']={'path':str(h/'profile.json'),'sha256':sha(h/'profile.json')}
# The host's glibc moved on (2.44+r78, 2026-11-20 15:12) after SVN23 was built; the three pinned shared objects are
# read from the r50 package instead (inputs/glibc-r50, from /var/cache/pacman/pkg), so the storage pins stay SVN23's.
for guest,pin in spec['storage'].items():
 if pin['path'] in ('/usr/lib/libc.so.6','/usr/lib/libm.so.6','/usr/lib/ld-linux-x86-64.so.2'):pin['path']=str(w/'native-audio-image-v21/inputs/glibc-r50')+pin['path']
 assert sha(Path(pin['path']))==pin['sha256'],guest
write(h/'spec.json',spec)
for source,name,digest in [('rootfs.squashfs','root.iso',r['sha256']),('rootfs.verity','hashes.iso',v['sha256'])]:
 assert sha(h/'root-image'/source)==digest
 result=subprocess.check_output(['python3',str(w/'enclave-apps-kvm-pair/kvm-box/guest/native/build_iso.py'),str(h/'root-image'/source),str(h/name),'--sha256',digest],text=True)
 with (h/(name+'.json')).open('x') as f:f.write(result)
builder=w/'enclave-kvm-guest-shares/isolation/m3/build-native.py'
go=tc/'toolchain/go-1.27.2/usr/lib/go/bin';zlib=tc/'storage-pinned/pkgs/zlib-1_1.3.2-4/usr/lib'
env={'PATH':f'{go}:/usr/local/bin:/usr/bin','HOME':str(h),'GOCACHE':str(h/'gocache-platform'),'GOMODCACHE':'/home/steven/go/pkg/mod','LD_LIBRARY_PATH':str(zlib)}
cpus=','.join(map(str,sorted(os.sched_getaffinity(0))[:2]))
command=['taskset','-c',cpus,'python3',str(builder),'--spec',str(h/'spec.json'),'--out',str(h/'platform')]
start=time.monotonic()
with (h/'build-platform.log').open('x') as log:subprocess.run(command,stdout=log,stderr=subprocess.STDOUT,check=True,env=env)
write(h/'build-platform-execution.json',{'command':command,'environment':env,'exitCode':0,'seconds':time.monotonic()-start})
assert read(h/'platform/build.json')['tools']==read(old/'platform/build.json')['tools']
assert (h/'check-platform.py').read_bytes()==(old/'check-platform.py').read_bytes()
for d in (old,h):
 if not (d/'platform/archive-check.json').exists():subprocess.run(['python3',str(h/'check-platform.py'),str(d/'platform')],check=True)
prior=read(old/'platform/archive-check.json')['entries'];current=read(h/'platform/archive-check.json')['entries']
changed=[n for n in sorted(prior.keys()|current.keys()) if prior.get(n)!=current.get(n)]
assert changed==['plat/native-boot','plat/native-desktop/profile.json'],changed
write(h/'platform-diff.json',{'passed':True,'against':'native-audio-image-v21 (SVN24, the live release)','changes':changed,'allOtherEntriesIdentical':True})
image=h/'platform/native.cpio.gz'
command=['node',str(w/'native-graphics-release/build-igvm.mjs'),str(image),sha(image),str(h/'igvm'),str(w/'nucbox-desktop-v2/kernel-desktop.elf'),'/home/steven/enclave-bench/nucbox-shield-20260929/openhcl-shield.cpio.gz']
write(h/'igvm-command.json',command)
start=time.monotonic()
with (h/'igvm-build.json').open('x') as out,(h/'igvm-build.stderr').open('x') as err:subprocess.run(command,stdout=out,stderr=err,check=True)
record=read(h/'igvm-build.json');assert record['candidate']['deterministic'] and all(m['changed'] for m in record['mutations'])
write(h/'igvm-execution.json',{'exitCode':0,'seconds':time.monotonic()-start})
print(json.dumps({'packaged':True,'signed':False,'unsignedImageSha256':record['candidate']['binSha'],'launchMeasurement':record['candidate']['digest'].lower(),'initrdSha256':sha(image),'profileSha256':sha(h/'profile.json'),'productionAdmission':False,'activated':False}),flush=True)
