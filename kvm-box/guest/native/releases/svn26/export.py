"""Export the SVN26 candidate (the 27B model on the model volume) without inheriting the live release's approvals.
Follows native-shield-image-v22/export.py."""
from pathlib import Path
import hashlib,json,os,shutil,subprocess
h=Path(__file__).resolve().parent;root=h.parent.parent
old=root/'outputs/kvm-box/native-platform-v22-shield-route';out=root/'outputs/kvm-box/native-platform-v23-27b-models'
pair=h.parent/'native-pair-image-v17';prev=h.parent/'native-shield-image-v22'
def read(p):return json.loads(p.read_text())
def sha(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def write(p,v):
    with p.open('x') as f:json.dump(v,f,indent=2);f.write('\n')
assert read(h/'build-platform-execution.json')['exitCode']==0 and read(h/'igvm-execution.json')['exitCode']==0
assert read(h/'independent-signature-verification.json')['passed'] and read(h/'probe/valid.result.json')['passed']
assert read(h/'platform-diff.json')['changes']==['plat/native-boot','plat/native-desktop/profile.json']
s=read(h/'signed-desktop/manifest.json');assert s['guestSvn']==26 and sha(h/'signed-desktop/image.igvm')==s['signedImageSha256']
prior=read(old/'candidate.json');assert prior['guestSvn']==25;p=read(h/'profile.json');meta=read(h/'root-image/image.json')
assert p['root']['imageSha256']==meta['image']['sha256'] and p['root']['rootHash']==meta['verity']['rootHash']
assert p['controllerSha256']==meta['sourcePins']['controller']==sha(pair/'build/shield-desktop.wasm')==prior['controllerSha256']
mi=read(h/'models.iso.json');assert sha(h/'models.iso')==mi['containerSha256'] and mi['payloadSha256']==p['models']['imageSha256'] and mi['payloadBytes']==p['models']['imageBytes']
assert p['models']!=read(prev/'profile.json')['models'] and {k:v for k,v in p.items() if k not in('models','root')}=={k:v for k,v in read(prev/'profile.json').items() if k not in('models','root')}
a=read(prev/'root-image/root-files.json');b=read(h/'root-image/root-files.json')
changed=sorted(k for k in set(a)|set(b) if a.get(k)!=b.get(k))
assert changed==['shield/model-volume.json','shield/mount-models.sh'],changed
write(h/'root-diff.json',{'passed':True,'against':'native-shield-image-v22 (the root image of SVN25)','changes':changed,'allOtherEntriesIdentical':True})
plat=h.parent/'enclave-kvm-guest-shares'
commit=subprocess.check_output(['git','-C',str(plat),'rev-parse','HEAD'],text=True).strip()
assert subprocess.check_output(['git','-C',str(plat),'status','--porcelain','--','isolation'],text=True)=='','platform source tree not clean'
shield=prior['shieldOverlayCommit']
out.mkdir(exist_ok=False);files={}
for name,source in [('native.cpio.gz',h/'platform/native.cpio.gz'),('native-igvm.bin',h/'signed-desktop/image.igvm'),('profile.json',h/'profile.json'),('root.iso',h/'root.iso'),('hashes.iso',h/'hashes.iso'),('shield-desktop.wasm',pair/'build/shield-desktop.wasm'),('models.iso',h/'models.iso')]:
    if name=='models.iso':os.link(source,out/name)
    else:shutil.copyfile(source,out/name);(out/name).chmod(0o444)
    digest=sha(out/name);assert digest==sha(source)
    files[name]={'file':name,'bytes':source.stat().st_size,'sha256':digest}
    if name.endswith('.iso'):
        iso=read(h/(name+'.json'))
        files[name].update({k:iso[k] for k in ['payloadOffset','payloadBytes','payloadSha256']})
for n in ['independent-signature-verification.json','platform-diff.json','root-diff.json','build-platform-execution.json']:shutil.copyfile(h/n,out/n)
shutil.copyfile(old/'controller-component-check.json',out/'controller-component-check.json')
shutil.copyfile(h/'probe/valid.result.json',out/'desktop-test.json')
shutil.copyfile(h/'root-image/image.json',out/'root-image.json')
shutil.copyfile(h/'platform/archive-check.json',out/'archive-check.json')
shutil.copyfile(h/'README.md',out/'README.md')
s['imageFile']='native-igvm.bin';write(out/'signing.json',s)
c={'schema':'enclave-native-platform-candidate/1','imageRole':'desktop',
 'scope':'SVN26 candidate. The model volume gains Qwen3.8-27B-UD-Q4_K_XL.gguf (kvm-box-public-models-v5: the two files of v4 and that one); the root image names the new volume in shield/model-volume.json and shield/mount-models.sh. Everything else is SVN25. The service lists no 27B model: llama-cli runs it on the processor.',
 'files':files,'profileSha256':sha(h/'profile.json'),'controllerSha256':p['controllerSha256'],'runtimeId':prior['runtimeId'],
 'vbsLaunchDigest':s['launchMeasurement'],'imageIdentity':prior['imageIdentity'],'guestSvn':26,'sourceUnsignedImageSha256':s['unsignedImageSha256'],
 'signatureLocallyVerified':True,'signingManifest':'signing.json','evidence':'independent-signature-verification.json',
 'appSourceCommit':prior['appSourceCommit'],'appSourceBranch':prior['appSourceBranch'],'xdoomSourceCommit':prior.get('xdoomSourceCommit'),'shieldOverlayCommit':shield,
 'graphicsSourceCommit':prior['graphicsSourceCommit'],'platformSourceCommit':commit,'platformSourceMainCommit':prior['platformSourceMainCommit'],
 'rootChanged':changed,'platformArchiveChangedAgainstV20':['plat/native-boot','plat/native-desktop/profile.json'],
 'isolatedNativeKvmQualified':True,'isolatedNativeEvidence':'desktop-test.json','isolatedNativeEvidenceIsOf':'this root image and this model volume, valid case only; the two corruption cases were not repeated',
 'isolatedGraphicsQualificationRepeated':False,'computeRouteExercised':prior['computeRouteExercised'],'computeOverlayExercisedInContainer':True,'gameExercisedInBootedGuest':prior.get('gameExercisedInBootedGuest',False),
 'purchasedShareContractExercisedInBootedGuest':False,'archiveRebuiltFromFreshCache':None,'previousCandidate':'native-platform-v22-shield-route','modelVolume':{'name':'kvm-box-public-models-v5','files':read(h/'volumes/kvm-box-public-models-v5/volume.json')['files'],'rootHash':p['models']['rootHash'],'gguf27Sha256':'3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e'},
 'finalImageRuntimeQualified':False,'hardwareAttestationChainVerified':False,'admissionEnabled':False,'activated':False,
 'hostOsExcluded':False,'containsTestPrograms':False,'controllerBundleIncluded':True,'startupReliabilityQualified':False,'controlledSlowStorageStartupPassed':False,
 'startupInnerDeadlineMs':prior['startupInnerDeadlineMs'],'startupOuterDeadlineMs':prior['startupOuterDeadlineMs'],
 'requiredBridgeSha256':prior['requiredBridgeSha256'],'driverSha256':prior['driverSha256']}
write(out/'candidate.json',c)
write(out/'manifest.json',{p.name:{'sha256':sha(p),'bytes':p.stat().st_size} for p in sorted(out.iterdir()) if p.is_file()})
print(json.dumps({'exported':str(out),'guestSvn':26,'signedImageSha256':s['signedImageSha256'],'admissionEnabled':False,'activated':False}))
