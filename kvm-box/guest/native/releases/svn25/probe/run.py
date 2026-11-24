"""Run the pair-again image's graph in a disposable KVM guest with no network device."""
from pathlib import Path
import hashlib,json,subprocess,sys
h=Path(__file__).resolve().parent;work=h.parent.parent;root=work.parent;image=h.parent/'root-image';case=sys.argv[1] if len(sys.argv)==2 else 'valid'
assert case in ['valid','data-corrupt','hash-corrupt']
assert not (h/(case+'.serial.log')).exists() and not (h/(case+'.result.json')).exists(),'preserve previous evidence'
initrd_sha=hashlib.sha256((h/'probe.initrd').read_bytes()).hexdigest()
meta=json.loads((image/'image.json').read_text());paths=[image/'rootfs.squashfs',image/'rootfs.verity',root/'outputs/kvm-box/release-v6.11-declines-candidate/models.img'];formats=['raw']*3
if case!='valid':
 idx=0 if case=='data-corrupt' else 1;overlay=h/(case+'.qcow2');assert not overlay.exists()
 subprocess.run(['qemu-img','create','-f','qcow2','-F','raw','-b',str(paths[idx]),str(overlay)],check=True)
 # Change one authenticated block only in a fresh disposable overlay.
 subprocess.run(['qemu-io','-f','qcow2','-c','write -P 0xa5 0 4096',str(overlay)],check=True)
 paths[idx]=overlay;formats[idx]='qcow2'
cmd=['qemu-system-x86_64','-machine','q35,accel=kvm','-cpu','host','-smp','2','-m','3072' if case=='valid' else '2048','-nodefaults','-no-reboot','-display','none','-monitor','none','-serial','file:'+str(h/(case+'.serial.log')),'-nic','none','-device','virtio-scsi-pci,id=scsi0','-kernel',str(work/'nucbox-desktop-v2/kernel-src/arch/x86/boot/bzImage'),'-initrd',str(h/'probe.initrd'),'-append','console=ttyS0 rdinit=/probe-init panic=-1 loglevel=4'+(' probe.expect-root-corruption=1' if case!='valid' else '')]
for i,(p,fmt) in enumerate(zip(paths,formats)):cmd+=['-drive',f'file={p},if=none,id=disk{i},format={fmt},readonly=on','-device',f'scsi-hd,drive=disk{i},bus=scsi0.0,channel=0,scsi-id={i},lun=0']
with (h/(case+'.qemu.log')).open('w') as log:
 p=subprocess.Popen(cmd,stdout=log,stderr=subprocess.STDOUT)
 try:code=p.wait(timeout=300)
 except subprocess.TimeoutExpired:
  p.terminate()
  try:code=p.wait(timeout=5)
  except subprocess.TimeoutExpired:p.kill();code=p.wait(timeout=5)
raw=(h/(case+'.serial.log')).read_bytes();lines=raw.decode(errors='replace').splitlines()
def report(prefix):
 found=[line[len(prefix):] for line in lines if line.startswith(prefix)]
 return json.loads(found[0]) if len(found)==1 else {'passed':False,'missingResult':True}
v=report('IMAGE_VERITY_RESULT=');m=report('MODEL_VERITY_RESULT=') if case=='valid' else None;graph=report('CONTROL_GRAPH_RESULT=') if case=='valid' else None
assets=None
if case=='valid':
 # The page's terminal files, as the guest received them through the whole chain, against the source.
 vendor=work/'enclave-apps-kvm-pair/kvm-box/src/vendor';facts=graph.get('facts',{})
 assets={name:{'served':facts.get(name),'source':{'bytes':(vendor/name).stat().st_size,'sha256':hashlib.sha256((vendor/name).read_bytes()).hexdigest()}} for name in ['xterm.js','xterm.css']}
 assets['passed']=all(assets[name]['served']==assets[name]['source'] for name in ['xterm.js','xterm.css'])
r={'schema':'kvm-box-native-root-image-kvm/1','case':case,'command':cmd,'initrdSha256':initrd_sha,'qemuExitCode':code,'rootImageSha256':meta['image']['sha256'],'rootHash':meta['verity']['rootHash'],'serialSha256':hashlib.sha256(raw).hexdigest(),'verity':v,'modelVerity':m,'graph':graph,'assets':assets,'passed':code==0 and v.get('passed') is True and (m is None or m.get('passed') is True) and (case!='valid' or graph.get('passed') is True and assets['passed'] is True),'scope':'Kernel integrity and the actual image graph with the owner page, its terminal, the desktop link and the pairing route with nothing to pair; pinned release runtime; synthetic worker, test runtime and gateway role adapters, synthetic owner credential; no GPU, admission, attestation or Moonlight streaming'}
(h/(case+'.result.json')).write_text(json.dumps(r,indent=2)+'\n');print(json.dumps({k:r[k] for k in r if k!='command'}));raise SystemExit(0 if r['passed'] else 1)
