// SVN26 (the 27B model on the model volume) activation files, made from the files that are live for SVN25
// (native-shield-image-v22/activation, checked by hash) and the exported v23 candidate. The controller, its CID, the
// catalog version (2, label 0.9.2), the release schema and the host bundle are unchanged. What changes: the image (the
// profile's model volume and root hash, launch measurement, signed image), all three media (the model disk is new
// this time and is staged with the others) and the SVN floor of its policy entry. Nothing here is activated by
// running this.
import fs from 'node:fs';import path from 'node:path';import assert from 'node:assert/strict';import {createHash} from 'node:crypto';
const SRC='/home/steven/Projects/enclave';
const {validateDesktopRelease,desktopRecordFor,deriveApprovedDesktop}=await import(SRC+'/relay/shield-desktop-release.mjs');
const {shieldImageCandidates}=await import(SRC+'/relay/shield-image-candidates.mjs');
const {validateDesktopAdmission}=await import(SRC+'/windows/vbslike/manager/desktop-admission.mjs');
const {validateDesktopPlatform}=await import(SRC+'/windows/vbslike/manager/desktop-platform.mjs');
const h=path.dirname(new URL(import.meta.url).pathname),w=path.dirname(path.dirname(h)),root=path.dirname(w);
const read=p=>JSON.parse(fs.readFileSync(p)),sha=b=>createHash('sha256').update(b).digest('hex');
const pinned=(p,want)=>{const b=fs.readFileSync(p);assert.equal(sha(b),want,p);return b};
// the relay code this was derived with is the code the relay runs (svn23.sh names these two)
pinned(SRC+'/relay/shield-desktop-release.mjs','8f4428f8dee0014fb1a8abe562b5418e9cb3d301e0006008af7b720eabde62ba');
pinned(SRC+'/relay/shield-desktop-derive.mjs','41d15e2752134047805e82d6106ecf446257ef607bdcb908dfa064991042681c');
const out=path.join(root,'outputs/kvm-box/native-platform-v23-27b-models'),c=read(path.join(out,'candidate.json'));
const CID=process.argv[2];assert.match(CID||'',/^baf[a-z2-7]{50,}$/,'usage: prepare.mjs <controller cid>');
const OLD_IGVM='8695919cb134018d3ce58e771ad9053a0586f543ac8ccea3bc2bbfbe41f0840e';
const live=path.join(w,'native-shield-image-v22/activation');
const APP='0x8e8a1340ca7e9ea1490ce15295d722c3f5186db9e0d0f1094661dbe919d8208c',DEPLOYMENT='0xf9bb979ae685c7f16f6c49df3f35f76ff38aca86b5ce3e76e02b83115360dd58';
const PORTS='http:8000,tcp:47984,tcp:47989,tcp:48010,udp:47998,udp:47999,udp:48000';
assert.equal(c.guestSvn,26);assert.equal(c.signatureLocallyVerified,true);
const oldPolicyRaw=pinned(path.join(live,'shield-policy.json'),'ae6326749c2b30b6317f41487bff932c448d560b3ba4e3e694a235a3226e31dc'),old=JSON.parse(oldPolicyRaw);
const oldConfig=JSON.parse(pinned(path.join(live,'desktop-config.json'),'e118ddf3b5192843dda3adf764c9cd42ecc0a18abc4d5a17b4a144066f42ee4d'));
const oldReleaseRaw=pinned(path.join(live,'desktop-release.json'),'506c9efbad67326a7976129540804b07d413bad74b3d718a5c597dc66657d5b0');
let env=pinned(path.join(live,'native-desktop-env.cmd'),'bcf8ee212a28f31b8c441b4d5bdaa990ef0af43956c499989efbc560004d5613').toString();
const prev=read(path.join(live,'prepared.json'));
assert.deepEqual(JSON.parse(oldReleaseRaw),old.desktop);assert.equal(old.desktop.igvmSha256,OLD_IGVM);assert.equal(old.desktop.schema,'enclave-desktop-release/2');
const component=pinned(path.join(out,'shield-desktop.wasm'),c.controllerSha256);
assert.equal(old.desktop.controllerCid,CID);assert.equal(old.desktop.controllerSha256,c.controllerSha256);
// desktop-config.json
const target='C:\\ProgramData\\Enclave\\native-desktop\\releases\\'+c.files['native-igvm.bin'].sha256;
const oldTarget='C:\\ProgramData\\Enclave\\native-desktop\\releases\\'+OLD_IGVM;
assert.equal(oldConfig.imagePath,oldTarget+'\\native-igvm.bin');
const config=structuredClone(oldConfig);
config.imagePath=target+'\\native-igvm.bin';
Object.assign(config.platform,{igvmSha256:c.files['native-igvm.bin'].sha256,profileSha256:c.profileSha256,profileText:fs.readFileSync(path.join(out,'profile.json'),'utf8')});
assert.equal(sha(config.platform.profileText),c.profileSha256);
for(const media of config.platform.media){
 const f=c.files[media.role+'.iso'];
 for(const k of ['bytes','sha256','payloadBytes','payloadSha256','payloadOffset'])media[k]=f[k];
 media.path=target+'\\'+media.role+'.iso';
}
assert.equal(oldConfig.bridge.sha256,c.requiredBridgeSha256);
config.bridge={exe:target+'\\shielded-bridge.exe',sha256:c.requiredBridgeSha256};
const admission=validateDesktopAdmission({platform:config.platform,runtime:config.runtime});assert.equal(admission.platform.profileSha256,c.profileSha256);
validateDesktopPlatform(config.platform);
for(const k of ['runtime','workerPort','workerBudgetGb'])assert.deepEqual(config[k],oldConfig[k]);
// what differs from the live configuration is the image, the three media and the folder they are in
{const strip=x=>{const y=structuredClone(x);delete y.imagePath;delete y.bridge.exe;for(const k of ['igvmSha256','profileSha256','profileText'])delete y.platform[k];y.platform.media=[];return y};
 assert.deepEqual(strip(config),strip(oldConfig));assert.deepEqual(config.platform.media.map(m=>m.role),oldConfig.platform.media.map(m=>m.role))}
// release and policy: the live block with the new image
const release=validateDesktopRelease({...old.desktop,igvmSha256:c.files['native-igvm.bin'].sha256,profileSha256:c.profileSha256,launchMeasurement:c.vbsLaunchDigest.toLowerCase()});
assert.equal(release.runtimeId,c.runtimeId);assert.equal(release.schema,'enclave-desktop-release/2');
assert.deepEqual(release.policy,old.desktop.policy);
assert.deepEqual(Object.keys(release).filter(k=>JSON.stringify(release[k])!==JSON.stringify(old.desktop[k])).sort(),['igvmSha256','launchMeasurement','profileSha256']);
const prior=old.images.filter(x=>x.nativeDesktop);assert.equal(prior.length,1);assert.equal(prior[0].minimumGuestSvn,25);assert.equal(prior[0].igvmSha256,OLD_IGVM);
const image={...prior[0],measurement:release.launchMeasurement,igvmSha256:release.igvmSha256,profileSha256:c.profileSha256,controllerSha256:c.controllerSha256,identity:c.imageIdentity,minimumGuestSvn:26};
assert.deepEqual(image.identity,prior[0].identity);
assert.deepEqual(Object.keys(image).filter(k=>JSON.stringify(image[k])!==JSON.stringify(prior[0][k])).sort(),['igvmSha256','measurement','minimumGuestSvn','profileSha256']);
const policy={...old,images:old.images.map(x=>x.nativeDesktop?image:x),desktop:release};
assert.deepEqual(shieldImageCandidates(policy,{runtimeId:c.runtimeId,requiresDesktop:true}),[image]);
assert.deepEqual(Object.keys(policy).filter(k=>JSON.stringify(policy[k])!==JSON.stringify(old[k])).sort(),['desktop','images']);
// every other image answers as before
const legacy=old.images.filter(x=>!x.nativeDesktop);assert.equal(legacy.length,12);
for(const p of legacy)assert.deepEqual(shieldImageCandidates(policy,{runtimeId:p.runtimeId}),shieldImageCandidates(old,{runtimeId:p.runtimeId}));
// the record the node derives for the deployment, at the shares it was bought with: the app identity moves with the image
const req={release,app:APP,version:2,cid:CID,memMiB:8192,ports:PORTS,deploymentId:DEPLOYMENT,cpuMilli:170,gpuMilli:100,config:{egress:'public-web',wasi:'0.2'}};
const next=deriveApprovedDesktop({release,record:desktopRecordFor(req),component});
assert.equal(next.record.desktop.gpuMilli,100);
const was=deriveApprovedDesktop({release:validateDesktopRelease(old.desktop),record:desktopRecordFor({...req,release:validateDesktopRelease(old.desktop)}),component});
assert.equal(was.appId,prev.appId);assert.equal(was.recordSha256,prev.recordSha256);
// write
const text=v=>JSON.stringify(v,null,2)+'\n',files={'desktop-config.json':text(config),'desktop-release.json':text(release),'shield-policy.json':text(policy),'derived.json':text({appId:next.appId,recordSha256:next.recordSha256,record:next.record})};
assert(env.includes(OLD_IGVM)&&env.includes('SVN25'));
env=env.replaceAll(OLD_IGVM,release.igvmSha256).replaceAll(sha(fs.readFileSync(path.join(live,'desktop-config.json'))),sha(files['desktop-config.json'])).replaceAll(sha(oldReleaseRaw),sha(files['desktop-release.json'])).replace(/^rem Native desktop SVN25.*$/m,'rem Native desktop SVN26 (the model volume gains Qwen3.8-27B-UD-Q4_K_XL.gguf; everything else is the release before it). Not hardware-qualified.');
assert(!env.includes(OLD_IGVM)&&!env.includes('e118ddf3')&&!env.includes('506c9efb')&&env.includes('SVN26')&&!env.includes('SVN25'));
files['native-desktop-env.cmd']=env;
for(const [n,x] of Object.entries(files))fs.writeFileSync(path.join(h,n),x,{flag:'wx'});
const hashes=Object.fromEntries(Object.entries(files).map(([n,x])=>[n,sha(x)]));
const stage=Object.fromEntries(['native-igvm.bin','root.iso','hashes.iso','models.iso','profile.json'].map(n=>[n,{bytes:c.files[n].bytes,sha256:c.files[n].sha256}]));
for(const n of ['desktop-config.json','desktop-release.json','native-desktop-env.cmd','shield-policy.json'])stage[n]={bytes:Buffer.byteLength(files[n]),sha256:hashes[n]};
stage['shielded-bridge.exe']={copyFrom:oldTarget+'\\shielded-bridge.exe',sha256:c.requiredBridgeSha256};
fs.writeFileSync(path.join(h,'prepared.json'),text({target,previousTarget:oldTarget,guestSvn:26,imageSha256:release.igvmSha256,profileSha256:release.profileSha256,launchMeasurement:release.launchMeasurement,controllerCid:CID,controllerSha256:release.controllerSha256,catalog:{app:APP,version:2,label:'0.9.2'},deployment:DEPLOYMENT,purchased:{cpuMilli:170,gpuMilli:100},appId:next.appId,previousAppId:was.appId,recordSha256:next.recordSha256,ownerSignatureNeeded:false,releaseSchema:release.schema,hostBundle:prev.hostBundle,hostBundleChanges:false,oldPolicySha256:sha(oldPolicyRaw),files:hashes,stage}),{flag:'wx'});
console.log(JSON.stringify({prepared:true,appId:next.appId,previousAppId:was.appId,recordSha256:next.recordSha256,files:hashes}));
