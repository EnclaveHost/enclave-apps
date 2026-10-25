#!/usr/bin/env python3
"""Build two separately measured images. Requires existing Enclave kernel/firmware/toolchain."""
import argparse,hashlib,json,os,secrets,shutil,subprocess
from pathlib import Path

def run(args,**kw):return subprocess.run([str(x) for x in args],check=True,**kw)
def sha(p):
 h=hashlib.sha256()
 with open(p,'rb') as f:
  for b in iter(lambda:f.read(1<<20),b''):h.update(b)
 return h.hexdigest()
def copy(a,b):b.parent.mkdir(parents=True,exist_ok=True);shutil.copy2(a,b)
def unpack_stream(stream,root):
 # Hash exactly the exported tar bytes while extracting, without retaining a
 # second multi-gigabyte rootfs archive alongside the extracted tree.
 digest=hashlib.sha256()
 process=subprocess.Popen(['tar','--no-same-owner','-xf','-','-C',str(root)],stdin=subprocess.PIPE)
 try:
  for block in iter(lambda:stream.read(1<<20),b''):
   digest.update(block);process.stdin.write(block)
  process.stdin.close()
  if process.wait():raise RuntimeError('rootfs extraction failed')
 except BaseException:
  if process.poll() is None:process.kill()
  process.wait()
  if not process.stdin.closed:process.stdin.close()
  raise
 return digest.hexdigest()
def install_volume(source,destination,link=False):
 # Public model volumes are immutable and independently authenticated at boot.
 # An existing destination is never truncated, particularly if it is a link.
 if destination.exists():
  if sha(source)!=sha(destination):raise ValueError('Existing model volume differs')
  return
 if link:os.link(source,destination)
 else:copy(source,destination)
def pack(root,out,compression='gzip'):
 for p in root.rglob('*'):os.utime(p,(0,0),follow_symlinks=False)
 entries=sorted(str(p.relative_to(root)) for p in root.rglob('*'))
 with open(out,'wb') as f:
  a=subprocess.Popen(['cpio','-o','-H','newc','--reproducible','--owner=0:0','--quiet'],cwd=root,stdin=subprocess.PIPE,stdout=subprocess.PIPE)
  compressor=['gzip','-n','-3'] if compression=='gzip' else ['xz','--threads=1','--check=crc32','-3']
  z=subprocess.Popen(compressor,stdin=a.stdout,stdout=f);a.stdout.close();a.communicate(('\n'.join(entries)+'\n').encode())
  if a.returncode or z.wait():raise RuntimeError('packing failed')
def main():
 ap=argparse.ArgumentParser();ap.add_argument('--compute-config',type=Path,required=True);ap.add_argument('--enclave',type=Path,required=True);ap.add_argument('--work',type=Path,required=True);ap.add_argument('--output',type=Path,required=True);ap.add_argument('--desktop-oci',default='enclave-kvm-box-desktop:0.3.0');ap.add_argument('--version');ap.add_argument('--shield-volume',type=Path);ap.add_argument('--link-model-volume',action='store_true',help='hard-link the immutable public model volume on the same filesystem');ap.add_argument('--desktop-memory-mib',type=int,choices=[4096,6144,8192],default=4096);ap.add_argument('--compression',choices=['gzip','xz'],default='gzip',help='xz needs CONFIG_RD_XZ in the guest kernel and reduces firmware initrd allocation');a=ap.parse_args()
 app=Path(__file__).resolve().parents[1];enclave=a.enclave.resolve();w=a.work.resolve();out=a.output.resolve();w.mkdir(parents=True,exist_ok=True);out.mkdir(parents=True,exist_ok=True)
 suffix='.cpio.xz' if a.compression=='xz' else '.cpio.gz'
 # Never silently rotate the user's credential when rebuilding an image.
 cred=w/'owner-password';
 if not cred.exists():cred.write_text(secrets.token_urlsafe(32)+'\n');cred.chmod(0o600)
 access=hashlib.sha256(cred.read_text().strip().encode()).hexdigest()
 env=subprocess.check_output(['bash','-c','. "$1/isolation/m1/domain.env"; printf "%s\\n%s\\n%s\\n%s\\n" "$KERNEL" "$OVMF" "$GUEST_KREL" "$APPEND"','bash',str(enclave)],text=True).splitlines();kernel,firmware,krel,append=env
 for src,name in [(kernel,'vmlinuz'),(firmware,'OVMF.amdsev.fd')]:copy(src,out/name)
 run(['cargo','build','--release','--target','wasm32-wasip2','--manifest-path',app/'Cargo.toml','--target-dir',w/'cargo'])
 copy(w/'cargo/wasm32-wasip2/release/kvm-box.wasm',out/'kvm-box.wasm')
 goenv={**os.environ,'CGO_ENABLED':'0'}
 for cwd,target,bin in [(app/'guest','.',w/'helper'),(enclave/'isolation/m2','./front',w/'front'),(enclave/'isolation/m2','./fwd',w/'fwd'),(enclave/'isolation/contract','./cmd/bundle',w/'bundle')]:run(['go','build','-trimpath','-buildvcs=false','-ldflags=-s -w -buildid=','-o',bin,target],cwd=cwd,env=goenv)
 run(['gcc','-static','-O2','-Wall','-Wextra','-Werror',app/'guest/init.c','-o',w/'init'])
 run([w/'bundle','build','-label','kvm-box','-world','wasi:cli','-http','8000','-mem','512','-vcpus','1',out/'kvm-box.wasm',out/'kvm-box.bundle'])
 roots={x:w/(x+'-root') for x in ['controller','desktop']}
 for root in roots.values():
  if root.exists():shutil.rmtree(root)
  root.mkdir()
 cid=subprocess.check_output(['docker','create',a.desktop_oci],text=True).strip()
 try:
  export=subprocess.Popen(['docker','export',cid],stdout=subprocess.PIPE)
  try:
   rootfs_sha=unpack_stream(export.stdout,roots['desktop'])
   if export.wait():raise RuntimeError('rootfs export failed')
  finally:
   export.stdout.close()
   if export.poll() is None:export.kill()
   export.wait()
 finally:run(['docker','rm',cid],stdout=subprocess.DEVNULL)
 volumes=[]
 if a.shield_volume:
  volume=json.loads((a.shield_volume/'volume.json').read_text())
  measured=json.loads((roots['desktop']/'shield/model-volume.json').read_text())
  if measured['verity']!=volume['verity']:raise ValueError('Measured model-volume policy mismatch')
  install_volume(a.shield_volume/'volume.img',out/'models.img',a.link_model_volume)
  volumes=[{'file':'models.img','sha256':sha(out/'models.img'),'verity':volume['verity']}]
 elif (roots['desktop']/'shield').exists():raise ValueError('Shield desktop requires authenticated model storage')
 shutil.rmtree(roots['desktop']/'dev');(roots['desktop']/'dev').mkdir()
 ctrl=roots['controller'];(ctrl/'bin').mkdir();copy(roots['desktop']/'bin/busybox',ctrl/'bin/busybox')
 for n in ['sh','cat','mkdir']:os.symlink('busybox',ctrl/'bin'/n)
 run([enclave/'isolation/m4/runtime-set.sh','compose',ctrl/'rt'])
 copy(out/'kvm-box.wasm',ctrl/'app.wasm');copy(out/'kvm-box.bundle',ctrl/'app.bundle')
 ctrl_id=sha(out/'kvm-box.bundle');(ctrl/'app.sha256').write_text(ctrl_id+'\n')
 # The service policy and verifier are part of the measured image.
 compute=roots['desktop']/'compute';compute.mkdir()
 for n in ['services.json','vcek.der','amd-chain.pem','min-tcb.json']:copy(a.compute_config/n,compute/n)
 for n,dest in [('compute.mjs','compute.mjs'),('compute.html','index.html'),('native.html','native.html')]:copy(app/'guest'/n,compute/dest)
 for n in ['isolation/m2/judge.mjs','isolation/contract/runtime.mjs','relay/snp-verify.mjs']:copy(enclave/n,compute/'enclave'/n)
 compute_hashes={str(p.relative_to(compute)):sha(p) for p in compute.rglob('*') if p.is_file()}
 descriptor={'compute':compute_hashes,'schema':'kvm-box-desktop-image/1','architecture':'x86_64','rootfsSha256':rootfs_sha,'inputs':{p.name:sha(p) for p in [w/'init',w/'helper',w/'front',app/'guest/desktop-start.sh',app/'guest/desktop-session.sh',app/'guest/welcome.html',app/'guest/sunshine.conf',app/'guest/sunshine-apps.json',app/'guest/xorg.conf']},'accessSha256':access}
 descriptor_bytes=json.dumps(descriptor,sort_keys=True,separators=(',',':')).encode();desktop_id=hashlib.sha256(descriptor_bytes).hexdigest()
 (out/'desktop-inputs.json').write_bytes(descriptor_bytes+b'\n');(roots['desktop']/'app.sha256').write_text(desktop_id+'\n')
 copy(out/'desktop-inputs.json',roots['desktop']/'desktop-inputs.json')
 for name,root in roots.items():
  for d in ['proc','sys','dev','tmp','run','root']:(root/d).mkdir(exist_ok=True)
  for bin in ['init','front','helper']:copy(w/bin,root/bin);(root/bin).chmod(0o755)
  copy(app/f'guest/{name}-start.sh',root/'start.sh');(root/'start.sh').chmod(0o755)
  (root/'access.sha256').write_text(access+'\n')
  modules=['vmw_vsock_virtio_transport','sev-guest']+(['virtio_net','uinput','uhid','hid-generic','evdev'] if name=='desktop' else [])
  if name=='desktop' and volumes:modules+=['virtio_blk','ext4','dm-verity','nf_tables','nft_reject_inet']
  order=[]
  for module in modules:
   dep=subprocess.check_output(['modprobe','-S',krel,'--show-depends',module],text=True)
   for line in dep.splitlines():
    if not line.startswith('insmod '):continue
    src=Path(line.split()[1]);dest='/modules/'+src.name
    if dest not in order:copy(src,root/dest.lstrip('/'));order.append(dest)
  (root/'module-order.txt').write_text('\n'.join(order)+'\n')
  if name=='desktop':
   for n in ['desktop-session.sh','welcome.html','sunshine.conf','sunshine-apps.json','xorg.conf']:copy(app/'guest'/n,root/n)
   (root/'desktop-session.sh').chmod(0o755)
  pack(root,out/(name+suffix),a.compression)
 records={}
 for name,vcpus,mem,app_id in [('controller',1,768,ctrl_id),('desktop',2,a.desktop_memory_mib,desktop_id)]:
  img=out/(name+suffix)
  measurement=subprocess.check_output([str(Path.home()/'.local/bin/sev-snp-measure'),'--mode','snp','--vcpus',str(vcpus),'--vcpu-family','26','--vcpu-model','2','--vcpu-stepping','1','--vmm-type','QEMU','--ovmf',str(out/'OVMF.amdsev.fd'),'--kernel',str(out/'vmlinuz'),'--initrd',str(img),'--append',append],text=True).strip()
  records[name]={'image':img.name,'sha256':sha(img),'appId':app_id,'measurement':measurement,'vcpus':vcpus,'memoryMiB':mem,'network':name=='desktop'}
  if name=='desktop' and volumes:records[name]['volumes']=volumes
 copy(ctrl/'rt/runtime.json',out/'controller-runtime.json')
 manifest={'schema':'kvm-box-release/1','version':a.version or ('0.4.0' if volumes else '0.3.0'),'kernel':{'file':'vmlinuz','sha256':sha(out/'vmlinuz')},'firmware':{'file':'OVMF.amdsev.fd','sha256':sha(out/'OVMF.amdsev.fd')},'commandLine':append,'images':records,'storage':'ephemeral guest RAM; immutable separate base images; dm-verity public models' if volumes else 'ephemeral guest RAM; immutable separate base images','catalogStatus':'unpublished; requires dedicated host-mediated VM broker'}
 (out/'release.json').write_text(json.dumps(manifest,indent=2)+'\n')
 print(json.dumps(manifest,indent=2))
if __name__=='__main__':main()
