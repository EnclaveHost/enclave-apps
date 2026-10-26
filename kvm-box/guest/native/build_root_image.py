#!/usr/bin/env python3
"""Build a separate native root image in an unprivileged private overlay.

This is a development packager, not runtime admission. The output descriptor
pins bytes and dm-verity parameters; a measured launcher must authenticate it.
The base tree is mounted read-only and never edited. No host device mapping is
created; veritysetup is used only for userspace format/verify.
"""
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import subprocess
import sys

FIELDS = {'schema', 'baseRoot', 'sdk', 'assets', 'output', 'mksquashfs', 'files'}
FILES = {
 'nsinit': ('usr/local/libexec/kvm-box/nsinit', 0o555),
 'supervisor': ('usr/local/libexec/kvm-box/supervisor', 0o555),
 'gateway': ('usr/local/libexec/kvm-box/gateway', 0o555),
 'inner': ('usr/local/libexec/kvm-box/inner', 0o555),
 'udev': ('usr/local/libexec/kvm-box/udev-launch', 0o555),
 'controller': ('usr/local/libexec/kvm-box/shield-desktop.wasm', 0o444),
 'readiness': ('usr/local/libexec/kvm-box/readiness.py', 0o444),
 'inputRules': ('usr/lib/udev/rules.d/70-kvm-box-input.rules', 0o444),
 'nftModule': ('usr/local/libexec/kvm-box/nft_ct.ko', 0o444),
 'xdoom': ('usr/local/bin/xdoom', 0o555),
 'service': ('shield/service.py', 0o444),
 'serviceConfig': ('shield/native-service.json', 0o444),
 'sunshineConfig': ('sunshine.conf', 0o444),
 'sunshineApps': ('sunshine-apps.json', 0o444),
}

AUTOMATION_FILES = {
 'automation': ('usr/local/libexec/kvm-box/automation', 0o555),
 'capture': ('usr/local/libexec/kvm-box/capture', 0o555),
}

def files_for_schema(schema):
 if schema == 'kvm-box-native-root-build/1': return dict(FILES)
 if schema == 'kvm-box-native-root-build/2': return {**FILES, **AUTOMATION_FILES}
 raise ValueError('unsupported build specification')

def digest(path):
 with open(path, 'rb') as f: return hashlib.file_digest(f, 'sha256').hexdigest()

def unique(pairs):
 out = {}
 for k, v in pairs:
  if k in out: raise ValueError('duplicate JSON key')
  out[k] = v
 return out

def read_json(path): return json.loads(Path(path).read_text(), object_pairs_hook=unique)
def canonical(value): return (json.dumps(value, sort_keys=True, separators=(',', ':'))+'\n').encode()

def walk_error(error): raise error

def tree(root):
 result = {}
 for directory, dirs, files in os.walk(root, followlinks=False, onerror=walk_error):
  for name in sorted(dirs+files):
   p=Path(directory)/name; rel=p.relative_to(root).as_posix(); s=p.lstat()
   if stat.S_ISLNK(s.st_mode): entry={'type':'symlink','target':os.readlink(p)}
   elif stat.S_ISDIR(s.st_mode): entry={'type':'directory','mode':stat.S_IMODE(s.st_mode)}
   elif stat.S_ISREG(s.st_mode): entry={'type':'file','mode':stat.S_IMODE(s.st_mode),'bytes':s.st_size,'sha256':digest(p)}
   else: raise ValueError('special file in image source: '+rel)
   result[rel]=entry
 return dict(sorted(result.items()))

def require_native_desktop_base(root):
 # A container export is not the customized VM base. Missing this fixed
 # configuration causes Xorg to seek a physical device and fail at boot.
 # Resolve from a manifest, without following source-tree symlinks to the host.
 manifest = tree(root)
 for name in ('xorg.conf', 'shield/model-volume.json'):
  entry = manifest.get(name)
  if not entry or entry['type'] != 'file' or not entry['bytes']:
   raise ValueError('missing native desktop base asset: ' + name)
 return manifest

def main():
 os.umask(0o022)
 if len(sys.argv)!=2: raise SystemExit('usage: build_root_image.py BUILD_SPEC.json')
 specpath=Path(sys.argv[1]).resolve(); spec=read_json(specpath)
 image_files=files_for_schema(spec.get('schema'))
 if set(spec)!=FIELDS or set(spec['files'])!=set(image_files):
  raise ValueError('unsupported build specification')
 if os.getuid()!=0:
  env={**os.environ,'KVMBOX_BUILD_PARENT_USERNS':os.readlink('/proc/self/ns/user'),'KVMBOX_BUILD_PARENT_MNTNS':os.readlink('/proc/self/ns/mnt')}
  raise SystemExit(subprocess.call(['unshare','--map-root-user','--mount','--fork',sys.executable,str(Path(__file__).resolve()),str(specpath)],env=env))
 for kind,key in [('user','KVMBOX_BUILD_PARENT_USERNS'),('mnt','KVMBOX_BUILD_PARENT_MNTNS')]:
  if not os.environ.get(key) or os.readlink('/proc/self/ns/'+kind)==os.environ[key]:raise ValueError('requires private unprivileged namespaces')
 uidmap=Path('/proc/self/uid_map').read_text().split()
 if len(uidmap)!=3 or uidmap[0]!='0' or int(uidmap[1])<=0 or uidmap[2]!='1':raise ValueError('host root refused')
 sources={}
 for key,item in spec['files'].items():
  if set(item)!={'path','sha256'} or not re.fullmatch('[0-9a-f]{64}',item['sha256']):raise ValueError('invalid file pin')
  p=Path(item['path']).resolve(strict=True)
  if not p.is_file() or digest(p)!=item['sha256']:raise ValueError('source pin mismatch: '+key)
  sources[key]=p
 tool=spec['mksquashfs']; toolpath=Path(tool['path']).resolve(strict=True)
 if set(tool)!={'path','sha256'} or digest(toolpath)!=tool['sha256']:raise ValueError('tool pin mismatch')
 base=Path(spec['baseRoot']).resolve(strict=True);sdk=Path(spec['sdk']).resolve(strict=True);assets=Path(spec['assets']).resolve(strict=True)
 manifest=read_json(sdk/'files.sha256.json')
 sdk_tree=tree(sdk)
 if not isinstance(manifest,dict) or not manifest:raise ValueError('empty SDK manifest')
 leaves={k for k,v in sdk_tree.items() if v['type']!='directory'}
 if leaves!=set(manifest)|{'files.sha256.json'}:raise ValueError('SDK manifest must cover every leaf')
 for name,expected in manifest.items():
  if not isinstance(name,str) or Path(name).is_absolute() or any(p in ('','.', '..') for p in name.split('/')) or not isinstance(expected,str) or not re.fullmatch('[0-9a-f]{64}',expected):raise ValueError('invalid SDK manifest entry')
  entry=sdk_tree[name]
  if entry['type']=='symlink' and Path(entry['target']).is_absolute():raise ValueError('absolute SDK symlink')
  p=(sdk/name).resolve(strict=True)
  if not p.is_relative_to(sdk) or digest(p)!=expected:raise ValueError('SDK manifest mismatch')
 asset_manifest=tree(assets)
 if any(v['type']=='symlink' for v in asset_manifest.values()):raise ValueError('UI must already be flattened')
 output=Path(spec['output']).resolve()
 if any(output==p or output.is_relative_to(p) for p in [base,sdk,assets]):raise ValueError('output may not modify an input tree')
 if any(c in str(p) for p in [base,output] for c in (',', ':', chr(92), chr(10))):raise ValueError('unsafe overlay path syntax')
 output.mkdir(mode=0o700,exist_ok=False)
 (output/'build-inputs.json').write_bytes(canonical(spec))
 subprocess.run(['mount','--make-rprivate','/'],check=True)
 for n in ['lower','upper','work','merged']:(output/n).mkdir()
 lower=output/'lower';merged=output/'merged'; mounted=[];rootfd=-1
 try:
  subprocess.run(['mount','--bind',str(base),str(lower)],check=True);mounted.append(lower)
  subprocess.run(['mount','-o','remount,bind,ro',str(lower)],check=True)
  if not os.statvfs(lower).f_flag & os.ST_RDONLY:raise ValueError('base is not read-only')
  # Record a complete content/metadata identity of the base and compare after.
  base_manifest=require_native_desktop_base(lower);base_identity=hashlib.sha256(canonical(base_manifest)).hexdigest()
  (output/'base-files.json').write_bytes(canonical(base_manifest))
  subprocess.run(['mount','-t','overlay','overlay','-o',f'lowerdir={lower},upperdir={output}/upper,workdir={output}/work,userxattr',str(merged)],check=True);mounted.append(merged)
  rootfd=os.open(merged,os.O_PATH|os.O_DIRECTORY|os.O_CLOEXEC)
  def parent_fd(rel):
   fd=os.dup(rootfd);parts=rel.split('/')
   if any(not p or p in ['.','..'] for p in parts):raise ValueError('invalid image path')
   try:
    for part in parts[:-1]:
     try:os.mkdir(part,0o755,dir_fd=fd)
     except FileExistsError:pass
     nxt=os.open(part,os.O_PATH|os.O_DIRECTORY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=fd);os.close(fd);fd=nxt
    return fd,parts[-1]
   except BaseException:os.close(fd);raise
  def remove(rel):
   fd,name=parent_fd(rel)
   try:
    try:s=os.stat(name,dir_fd=fd,follow_symlinks=False)
    except FileNotFoundError:return
    if stat.S_ISDIR(s.st_mode):shutil.rmtree(name,dir_fd=fd)
    else:os.unlink(name,dir_fd=fd)
   finally:os.close(fd)
  def write(rel,data,mode=0o444):
   fd,name=parent_fd(rel)
   try:
    try:os.unlink(name,dir_fd=fd)
    except FileNotFoundError:pass
    outfd=os.open(name,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW|os.O_CLOEXEC,mode,dir_fd=fd)
    with os.fdopen(outfd,'wb') as f:f.write(data);os.fchmod(f.fileno(),mode)
    os.utime(name,(0,0),dir_fd=fd,follow_symlinks=False)
   finally:os.close(fd)
  def empty(rel,mode):
   remove(rel);fd,name=parent_fd(rel)
   try:os.mkdir(name,mode,dir_fd=fd);os.chmod(name,mode,dir_fd=fd,follow_symlinks=False)
   finally:os.close(fd)
  # Writable session/device/model state is supplied afresh by the launcher.
  for rel,mode in [('home',0o755),('home/desktop',0o700),('root',0o700),('run',0o755),('tmp',0o1777),('var/tmp',0o1777),('var/log',0o755),('var/backups',0o755),('var/mail',0o755),('var/spool',0o755),('var/lib/private',0o700),('var/cache/private',0o700),('dev',0o755),('proc',0o755),('sys',0o755),('models',0o755),('etc/udev/rules.d',0o755),('usr/lib/udev/rules.d',0o755)]:empty(rel,mode)
  # Fixed public image identity: no identifier from the build container/host.
  # Per-session identity can be supplied by a future measured launcher.
  for rel in ['etc/machine-id','var/lib/dbus/machine-id']:write(rel,b'7a25482197b340dfb14a17300cbbdd73\n')
  write('etc/hostname',b'kvm-box\n')
  write('etc/hosts',b'127.0.0.1 localhost\n::1 localhost ip6-localhost ip6-loopback\n127.0.1.1 kvm-box\n')
  for rel in ['var/lib/systemd/random-seed','var/lib/urandom/random-seed']:remove(rel)
  for rel in base_manifest:
   if rel.startswith('etc/ssh/ssh_host_'):remove(rel)
  remove('opt/shield');remove('usr/share/kvm-box/native-ui')
  # Fixed parents were checked without symlink traversal above.
  for src,rel in [(sdk,'opt/shield'),(assets,'usr/share/kvm-box/native-ui')]:
   fd,name=parent_fd(rel);os.close(fd)
   shutil.copytree(src,merged/rel,symlinks=True)
  if tree(merged/'opt/shield')!=sdk_tree:raise ValueError('copied SDK differs from verified source tree')
  if tree(merged/'usr/share/kvm-box/native-ui')!=asset_manifest:raise ValueError('copied UI differs from verified source tree')
  for key,(rel,mode) in image_files.items():write(rel,sources[key].read_bytes(),mode)
  write('etc/pulse/client.conf',b'autospawn = no\n')
  write('etc/resolv.conf',b'nameserver 127.0.0.2\noptions timeout:2 attempts:2\n')
  write('desktop.profile',b'KVM-BOX-MASKED/1\n')
  policy_path='usr/share/firefox-esr/distribution/policies.json'
  policyfd,policyname=parent_fd(policy_path)
  try:
   f=os.open(policyname,os.O_RDONLY|os.O_NOFOLLOW|os.O_CLOEXEC,dir_fd=policyfd)
   with os.fdopen(f) as policyfile:policies=json.load(policyfile,object_pairs_hook=unique)
  finally:os.close(policyfd)
  policies['policies']['Proxy']={'Mode':'manual','Locked':True,'SOCKSProxy':'127.0.0.2:1080','SOCKSVersion':5,'UseProxyForDNS':True,'Passthrough':''}
  write(policy_path,canonical(policies))
  # No privilege-bearing modes or file capabilities in the desktop payload.
  stripped=[]
  for directory,dirs,files in os.walk(merged,followlinks=False,onerror=walk_error):
   for name in dirs+files:
    p=Path(directory)/name;s=p.lstat()
    if not stat.S_ISLNK(s.st_mode) and s.st_mode & 0o6000:
     os.chmod(p,stat.S_IMODE(s.st_mode)&~0o6000,follow_symlinks=False);stripped.append(p.relative_to(merged).as_posix())
  image_manifest=tree(merged)
  (output/'root-files.json').write_bytes(canonical(image_manifest))
  os.close(rootfd);rootfd=-1
  image=output/'rootfs.squashfs'
  cmd=[str(toolpath),str(merged),str(image),'-noappend','-comp','xz','-b','1M','-processors','2','-mem','512M','-mkfs-time','0','-all-root','-no-xattrs','-no-progress']
  with (output/'squashfs-build.log').open('w') as log:subprocess.run(cmd,stdout=log,stderr=subprocess.STDOUT,check=True)
  if tree(lower)!=base_manifest:raise ValueError('base tree changed during build')
 finally:
  if rootfd>=0:os.close(rootfd)
  for p in reversed(mounted):subprocess.run(['umount',str(p)],check=True)
 # Integrity sidecar is deterministic and has no attacker-controlled superblock.
 image=output/'rootfs.squashfs';hashes=output/'rootfs.verity';hashes.touch(exist_ok=False)
 size=image.stat().st_size
 if not size or size%4096:raise ValueError('image must be block-aligned')
 image_sha=digest(image);salt=hashlib.sha256(b'kvm-box-native-root-verity/1\0'+bytes.fromhex(image_sha)).hexdigest();blocks=size//4096
 options=['--no-superblock','--format=1','--hash=sha256','--hash-offset=0','--data-block-size=4096','--hash-block-size=4096','--data-blocks='+str(blocks),'--salt='+salt]
 env={**os.environ,'LC_ALL':'C'}
 fmt=subprocess.run(['veritysetup','format',*options,str(image),str(hashes)],capture_output=True,text=True,env=env,check=True)
 (output/'verity-format.log').write_text(fmt.stdout+fmt.stderr)
 match=re.findall(r'^Root hash:\s*([0-9a-f]{64})\s*$',fmt.stdout,re.M)
 if len(match)!=1:raise ValueError('missing verity root hash')
 root_hash=match[0];verify=['veritysetup','verify',*options,str(image),str(hashes),root_hash]
 subprocess.run(verify,check=True,capture_output=True,env=env)
 negatives={}
 for p in [image,hashes]:
  with p.open('r+b') as f:
   original=f.read(1);f.seek(0);f.write(bytes([original[0]^1]));f.flush();os.fsync(f.fileno())
   try:r=subprocess.run(verify,capture_output=True,env=env,timeout=120);negatives[p.name]=r.returncode!=0
   finally:f.seek(0);f.write(original);f.flush();os.fsync(f.fileno())
  if not negatives[p.name]:raise ValueError('corrupted artifact was accepted')
 if digest(image)!=image_sha:raise ValueError('image restoration mismatch')
 subprocess.run(verify,check=True,capture_output=True,env=env)
 receipt={'schema':'kvm-box-native-root-image/1','baseTreeSha256':base_identity,'rootFilesSha256':digest(output/'root-files.json'),'image':{'file':image.name,'sha256':image_sha,'bytes':size,'filesystem':'squashfs','compression':'xz'},'verity':{'file':hashes.name,'sha256':digest(hashes),'bytes':hashes.stat().st_size,'rootHash':root_hash,'salt':salt,'format':1,'hash':'sha256','dataBlockSize':4096,'hashBlockSize':4096,'dataBlocks':blocks,'hashOffset':0,'superblock':False},'sourcePins':{k:v['sha256'] for k,v in spec['files'].items()},'sdkManifestSha256':digest(sdk/'files.sha256.json'),'sdkTreeSha256':hashlib.sha256(canonical(sdk_tree)).hexdigest(),'identityPolicy':'fixed-public-image-identity; no build-host identity','assetsTreeSha256':hashlib.sha256(canonical(asset_manifest)).hexdigest(),'strippedPrivilegeModes':sorted(stripped),'baseUnchanged':True,'userspaceVerityVerified':True,'corruptionRefused':negatives,'runtimeAdmission':False,'gpuAcceptance':False,'moonlightStreaming':False}
 (output/'image.json').write_bytes(canonical(receipt))
 image.chmod(0o444);hashes.chmod(0o444)
 for name in ['upper','work','merged','lower']:shutil.rmtree(output/name)
 print(json.dumps({'output':str(output),'imageSha256':image_sha,'rootHash':root_hash,'bytes':size}),flush=True)

if __name__=='__main__':main()
