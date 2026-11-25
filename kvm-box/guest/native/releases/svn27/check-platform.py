"""Independent initramfs reader: validate shipped bytes, not builder metadata alone."""
from pathlib import Path
import gzip,hashlib,json,stat,sys
folder=Path(sys.argv[1]);raw=gzip.decompress((folder/'native.cpio.gz').read_bytes());at=0;entries={}
while at<len(raw):
    assert raw[at:at+6]==b'070701',('bad newc magic',at)
    fields=[int(raw[at+6+i*8:at+14+i*8],16) for i in range(13)]
    ino,mode,uid,gid,nlink,mtime,size,dmaj,dmin,rmaj,rmin,nsize,check=fields
    at+=110;label=raw[at:at+nsize];assert label.endswith(b'\0');name=label[:-1].decode();at+=nsize;at=(at+3)&~3
    data=raw[at:at+size];assert len(data)==size;at+=size;at=(at+3)&~3
    if name=='TRAILER!!!':break
    assert name not in entries and uid==gid==mtime==rmaj==rmin==check==0
    assert name=='.' or (not name.startswith('/') and all(x not in ('','.','..') for x in name.split('/')))
    assert stat.S_ISDIR(mode) or stat.S_ISREG(mode),('nonregular input',name)
    if stat.S_ISDIR(mode):assert stat.S_IMODE(mode)==(0o1777 if name=='tmp' else 0o755) and size==0
    else:assert stat.S_IMODE(mode) in (0o444,0o555)
    entries[name]={'mode':mode,'bytes':size,'sha256':hashlib.sha256(data).hexdigest(),'data':data}
assert not any(raw[at:]),'nonzero trailing archive bytes'
required={'init','monitor','plat/domexec','plat/native-boot','plat/front','plat/secretrun','plat/native-desktop/profile.json','enclave_openhcl_tpm.builtin','dmsetup','veritysetup','lib64/ld-linux-x86-64.so.2'}
rt={'wasmtime','ld-linux-x86-64.so.2','libc.so.6','libgcc_s.so.1','libm.so.6','runtime.json'}
required|={'plat/rt/'+n for n in rt}
assert required<=entries.keys()
assert entries['enclave_openhcl_tpm.builtin']['data']==b'ENCLAVE-OPENHCL-TPM/1\n'
spec=json.loads((Path(__file__).parent/'spec.json').read_text())
for guest,pin in spec['storage'].items():assert entries[guest]['sha256']==pin['sha256']
for name,pin in spec['runtime'].items():assert entries['plat/rt/'+name]['sha256']==pin['sha256']
assert entries['plat/native-desktop/profile.json']['sha256']==spec['profile']['sha256']
for name,value in entries.items():
    if not stat.S_ISREG(value['mode']):continue
    assert name in required or name in spec['storage'],('unexpected file',name)
    if name not in {'enclave_openhcl_tpm.builtin','plat/rt/runtime.json','plat/native-desktop/profile.json'}:assert value['data'].startswith(b'\x7fELF')
for n in ['plat/native-desktop/nsinit','app.wasm','probe-init','monitor-probe','plat/domprobe','probe.ko']:
    assert n not in entries
policy=(Path(__file__).resolve().parent.parent/'enclave-kvm-guest/isolation/m3/monitor/native-network.nft').read_bytes()
assert policy in entries['monitor']['data'], 'monitor does not embed the expected policy'
r={'nativeNetworkPolicySha256':hashlib.sha256(policy).hexdigest(),'passed':True,'initrdSha256':hashlib.sha256((folder/'native.cpio.gz').read_bytes()).hexdigest(),'entries':{n:{k:v for k,v in e.items() if k!='data'} for n,e in entries.items()},'noTestPrograms':True,'externalResourcesNotEmbedded':True,'rootOwnershipAndModesVerified':True}
(folder/'archive-check.json').write_text(json.dumps(r,indent=2)+'\n');print(json.dumps({k:v for k,v in r.items() if k!='entries'}))
