"""Export the SVN27 candidate (the sixteen-lane SwiGLU frontend) without inheriting the live release's approvals.
Follows native-models-image-v23/export.py."""
from pathlib import Path
import hashlib,json,os,shutil,subprocess
h=Path(__file__).resolve().parent;root=h.parent.parent
old=root/'outputs/kvm-box/native-platform-v23-27b-models';out=root/'outputs/kvm-box/native-platform-v24-swiglu16'
pair=h.parent/'native-pair-image-v17';prev=h.parent/'native-models-image-v23'
def read(p):return json.loads(p.read_text())
def sha(p):
    with p.open('rb') as f:return hashlib.file_digest(f,'sha256').hexdigest()
def write(p,v):
    with p.open('x') as f:json.dump(v,f,indent=2);f.write('\n')
assert read(h/'build-platform-execution.json')['exitCode']==0 and read(h/'igvm-execution.json')['exitCode']==0
assert read(h/'independent-signature-verification.json')['passed'] and read(h/'probe/valid.result.json')['passed']
assert read(h/'platform-diff.json')['changes']==['plat/native-boot','plat/native-desktop/profile.json']
s=read(h/'signed-desktop/manifest.json');assert s['guestSvn']==27 and sha(h/'signed-desktop/image.igvm')==s['signedImageSha256']
prior=read(old/'candidate.json');assert prior['guestSvn']==26;p=read(h/'profile.json');meta=read(h/'root-image/image.json')
assert p['root']['imageSha256']==meta['image']['sha256'] and p['root']['rootHash']==meta['verity']['rootHash']
assert p['controllerSha256']==meta['sourcePins']['controller']==sha(pair/'build/shield-desktop.wasm')==prior['controllerSha256']
mi=read(h/'models.iso.json');assert sha(h/'models.iso')==mi['containerSha256'] and mi['payloadSha256']==p['models']['imageSha256'] and mi['payloadBytes']==p['models']['imageBytes']
assert p['root']!=read(prev/'profile.json')['root'] and {k:v for k,v in p.items() if k!='root'}=={k:v for k,v in read(prev/'profile.json').items() if k!='root'}
a=read(prev/'root-image/root-files.json');b=read(h/'root-image/root-files.json')
changed=sorted(k for k in set(a)|set(b) if a.get(k)!=b.get(k))
assert changed==['opt/shield/files.sha256.json','opt/shield/opencl/lib/libOpenCL.so.1','opt/shield/standard/inputs.json','opt/shield/standard/overlay-files.json','opt/shield/standard/source-declines/build-receipt.json','opt/shield/standard/source-declines/command.json'],changed
write(h/'root-diff.json',{'passed':True,'against':'native-models-image-v23 (the root image of SVN26)','changes':changed,'allOtherEntriesIdentical':True})
plat=h.parent/'enclave-kvm-guest-shares'
commit=subprocess.check_output(['git','-C',str(plat),'rev-parse','HEAD'],text=True).strip()
assert subprocess.check_output(['git','-C',str(plat),'status','--porcelain','--','isolation'],text=True)=='','platform source tree not clean'
shield='bb54abc0720d9809bd2c0ec27361c06b0b7e543b'
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
 'scope':'SVN27 candidate. The OpenCL frontend in the root image is rebuilt from enclave-apps main bb54abc: its SwiGLU glue kernel has a sixteen-lane form (same bits as the eight-lane one). Everything else is SVN26, the model volume included. Built and signed only; not staged.',
 'files':files,'profileSha256':sha(h/'profile.json'),'controllerSha256':p['controllerSha256'],'runtimeId':prior['runtimeId'],
 'vbsLaunchDigest':s['launchMeasurement'],'imageIdentity':prior['imageIdentity'],'guestSvn':27,'sourceUnsignedImageSha256':s['unsignedImageSha256'],
 'signatureLocallyVerified':True,'signingManifest':'signing.json','evidence':'independent-signature-verification.json',
 'appSourceCommit':prior['appSourceCommit'],'appSourceBranch':prior['appSourceBranch'],'xdoomSourceCommit':prior.get('xdoomSourceCommit'),'shieldOverlayCommit':shield,
 'graphicsSourceCommit':prior['graphicsSourceCommit'],'platformSourceCommit':commit,'platformSourceMainCommit':prior['platformSourceMainCommit'],
 'rootChanged':changed,'platformArchiveChangedAgainstV20':['plat/native-boot','plat/native-desktop/profile.json'],
 'isolatedNativeKvmQualified':True,'isolatedNativeEvidence':'desktop-test.json','isolatedNativeEvidenceIsOf':'this root image and the model volume of SVN26, valid case only; the two corruption cases were not repeated',
 'isolatedGraphicsQualificationRepeated':False,'computeRouteExercised':prior['computeRouteExercised'],'computeOverlayExercisedInContainer':True,'gameExercisedInBootedGuest':prior.get('gameExercisedInBootedGuest',False),
 'purchasedShareContractExercisedInBootedGuest':False,'archiveRebuiltFromFreshCache':None,'previousCandidate':'native-platform-v23-27b-models','modelVolume':{'name':'kvm-box-public-models-v5','files':read(h/'volumes/kvm-box-public-models-v5/volume.json')['files'],'rootHash':p['models']['rootHash'],'gguf27Sha256':'3f227079003add2511437e5b1e94812e363385225bf6a9b47b0054a72bc8b01e'},
 'finalImageRuntimeQualified':False,'hardwareAttestationChainVerified':False,'admissionEnabled':False,'activated':False,
 'hostOsExcluded':False,'containsTestPrograms':False,'controllerBundleIncluded':True,'startupReliabilityQualified':False,'controlledSlowStorageStartupPassed':False,
 'startupInnerDeadlineMs':prior['startupInnerDeadlineMs'],'startupOuterDeadlineMs':prior['startupOuterDeadlineMs'],
 'requiredBridgeSha256':prior['requiredBridgeSha256'],'driverSha256':prior['driverSha256']}
write(out/'candidate.json',c)
write(out/'manifest.json',{p.name:{'sha256':sha(p),'bytes':p.stat().st_size} for p in sorted(out.iterdir()) if p.is_file()})
print(json.dumps({'exported':str(out),'guestSvn':27,'signedImageSha256':s['signedImageSha256'],'admissionEnabled':False,'activated':False}))
