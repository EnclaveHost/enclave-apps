"""Initrd for the disposable KVM probe of the pair-again image (SVN21): test adapters
around the real root image, with the release's pinned WebAssembly runtime."""
from pathlib import Path
import hashlib,json,re,stat,subprocess
h=Path(__file__).resolve().parent;work=h.parent.parent;image=h.parent/'root-image';meta=json.loads((image/'image.json').read_text())
native=work/'enclave-apps-kvm-pair/kvm-box/guest/native';tests=work/'enclave-apps-kvm-pair/kvm-box/tests'
v=meta['verity'];(h/'verity-params.h').write_text(f'#define DATA_BLOCKS "{v["dataBlocks"]}"\n#define VERITY_SALT "{v["salt"]}"\n#define VERITY_ROOT "{v["rootHash"]}"\n#define IMAGE_BYTES {meta["image"]["bytes"]}ULL\n')
modelpath=h.parent/'base/shield/model-volume.json'
model=json.loads(modelpath.read_text())['verity'];rootfiles=json.loads((image/'root-files.json').read_text())
assert hashlib.sha256(modelpath.read_bytes()).hexdigest()==rootfiles['shield/model-volume.json']['sha256']
with (h/'verity-params.h').open('a') as header:
 for name,key in [('MODEL_BLOCKS','dataBlocks'),('MODEL_HASH_START','hashStartBlock'),('MODEL_ROOT','root'),('MODEL_SALT','salt')]:header.write(f'#define {name} "{model[key]}"\n')
 header.write(f'#define MODEL_BYTES {model["dataBlocks"]*4096}ULL\n')
cc=['gcc','-static','-O2','-Wall','-Wextra','-Werror']
subprocess.run(cc+['-Wno-misleading-indentation','-o',str(h/'probe-init'),str(h/'init.c')],check=True)
subprocess.run(cc+['-I'+str(native),'-o',str(h/'probe-runtime'),str(h/'runtime.c')],check=True)
subprocess.run(cc+['-o',str(h/'probe-gateway'),str(tests/'native_gateway_role_fixture.c')],check=True)
spec=json.loads((work/'shield-grid-native/spec.json').read_text())
entries={}
def add(name,path,mode=0o444):entries[name]=(Path(path).read_bytes(),stat.S_IFREG|mode)
def symlink(name,target):entries[name]=(target.encode(),stat.S_IFLNK|0o777)
add('probe-init',h/'probe-init',0o555);add('probe-chain.py',h/'driver.py')
add('probe-runtime',h/'probe-runtime',0o555);add('probe-gateway',h/'probe-gateway',0o555)
add('probe-network.nft',work/'native-nsinit-vm/network.nft')
# The runtime the release ships (platform spec pins), not the host's.
for name,pin in sorted(spec['runtime'].items()):
 assert hashlib.sha256(Path(pin['path']).read_bytes()).hexdigest()==pin['sha256'],name
 add('rt/'+name,pin['path'],0o555)
add('veritysetup','/usr/sbin/veritysetup',0o555)
add('lib64/ld-linux-x86-64.so.2','/lib64/ld-linux-x86-64.so.2',0o555)
ldd=subprocess.check_output(['ldd','/usr/sbin/veritysetup'],text=True)
for name in sorted(set(re.findall(r'(/[^\s]+)\s+\(',ldd))):add(name.lstrip('/'),name,0o555)
symlink('bin','usr/bin');symlink('sbin','usr/sbin');symlink('lib','usr/lib')
for name in list(entries):
 for p in Path(name).parents:
  if str(p)!='.':entries.setdefault(str(p),(b'',stat.S_IFDIR|0o755))
for name in ['dev','proc','sys','run','etc','opt','usr','tmp','lib64']:entries.setdefault(name,(b'',stat.S_IFDIR|(0o1777 if name=='tmp' else 0o755)))
with (h/'probe.initrd').open('wb') as out:
 for i,(name,(data,mode)) in enumerate(sorted(entries.items(),key=lambda kv:(len(Path(kv[0]).parts),kv[0]))+[('TRAILER!!!',(b'',0))]):
  label=name.encode()+b'\0';fields=[1+i,mode,0,0,2 if stat.S_ISDIR(mode) else 1,0,len(data),0,0,0,0,len(label),0]
  out.write(b'070701'+b''.join(f'{x:08x}'.encode() for x in fields)+label+b'\0'*(-(110+len(label))%4));out.write(data+b'\0'*(-len(data)%4))
def sha(p):
 with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
r={'scope':'Read-only immutable root via kernel dm-verity; real model volume; real supervisor, nsinit, gateways, automation service and controller from the image; pinned release runtime; synthetic worker, test runtime and gateway role adapters, synthetic owner credential; no GPU, admission, attestation or streaming','initrdSha256':sha(h/'probe.initrd'),'initrdBytes':(h/'probe.initrd').stat().st_size,'rootImageSha256':meta['image']['sha256'],'rootHash':v['rootHash'],'files':{k:hashlib.sha256(data).hexdigest() for k,(data,mode) in entries.items() if stat.S_ISREG(mode)}}
(h/'image.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps({k:v for k,v in r.items() if k!='files'}))
