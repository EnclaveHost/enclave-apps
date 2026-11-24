// SVN24 (the Doom-audio image) activation files, made from the files that are live for SVN23
// (native-direct-image-v20/activation, checked by hash; the relay's policy and the NucBox's own were both read as that
// policy on 2026-11-24) and the exported v21 candidate. The controller, its CID, the catalog version (2, label 0.9.2),
// the release schema and the host bundle are unchanged. What changes: the image (root hash in the profile, launch
// measurement, signed image) and the SVN floor of its policy entry. Nothing here is activated by running this.
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
const out=path.join(root,'outputs/kvm-box/native-platform-v22-shield-route'),c=read(path.join(out,'candidate.json'));
const CID=process.argv[2];assert.match(CID||'',/^baf[a-z2-7]{50,}$/,'usage: prepare.mjs <controller cid>');
const OLD_IGVM='a5a8c9fcb54dd937c2ba51666206b336d496adc4d4f708995a0fff0edb1b4e79';
const live=path.join(w,'native-audio-image-v21/activation');
const APP='0x8e8a1340ca7e9ea1490ce15295d722c3f5186db9e0d0f1094661dbe919d8208c',DEPLOYMENT='0xf9bb979ae685c7f16f6c49df3f35f76ff38aca86b5ce3e76e02b83115360dd58';
const PORTS='http:8000,tcp:47984,tcp:47989,tcp:48010,udp:47998,udp:47999,udp:48000';
assert.equal(c.guestSvn,25);assert.equal(c.signatureLocallyVerified,true);
const oldPolicyRaw=pinned(path.join(live,'shield-policy.json'),'df70e1d955a6570ab38f8fcc95b44e2d6bea1a73edb0e399b7c986851fef1f48'),old=JSON.parse(oldPolicyRaw);
const oldConfig=JSON.parse(pinned(path.join(live,'desktop-config.json'),'4c8adc8d43ac2101fa753b158eb2f282bbd6820aab0b72cfc226952a2043dc71'));
const oldReleaseRaw=pinned(path.join(live,'desktop-release.json'),'12f229303facfa771d91f68560ee66df9a8368c4907c2b0f6aff035ce5c92f0b');
let env=pinned(path.join(live,'native-desktop-env.cmd'),'870cf6bf2e1fd4e4a9d47200006314171a2b228599a79dfc0113b540a8644595').toString();
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
 for(const k of ['bytes','sha256','payloadBytes','payloadSha256','payloadOffset']){if(media.role==='models')assert.equal(media[k],f[k]);else media[k]=f[k]}
 if(media.role!=='models')media.path=target+'\\'+media.role+'.iso';
}
assert.equal(oldConfig.bridge.sha256,c.requiredBridgeSha256);
config.bridge={exe:target+'\\shielded-bridge.exe',sha256:c.requiredBridgeSha256};
const admission=validateDesktopAdmission({platform:config.platform,runtime:config.runtime});assert.equal(admission.platform.profileSha256,c.profileSha256);
validateDesktopPlatform(config.platform);
for(const k of ['runtime','workerPort','workerBudgetGb'])assert.deepEqual(config[k],oldConfig[k]);
// what differs from the live configuration is the image, the two root media and the folder they are in
{const strip=x=>{const y=structuredClone(x);delete y.imagePath;delete y.bridge.exe;for(const k of ['igvmSha256','profileSha256','profileText'])delete y.platform[k];y.platform.media=y.platform.media.filter(m=>m.role==='models');return y};
 assert.deepEqual(strip(config),strip(oldConfig));assert.deepEqual(config.platform.media.map(m=>m.role),oldConfig.platform.media.map(m=>m.role))}
// release and policy: the live block with the new image
const release=validateDesktopRelease({...old.desktop,igvmSha256:c.files['native-igvm.bin'].sha256,profileSha256:c.profileSha256,launchMeasurement:c.vbsLaunchDigest.toLowerCase()});
assert.equal(release.runtimeId,c.runtimeId);assert.equal(release.schema,'enclave-desktop-release/2');
assert.deepEqual(release.policy,old.desktop.policy);
assert.deepEqual(Object.keys(release).filter(k=>JSON.stringify(release[k])!==JSON.stringify(old.desktop[k])).sort(),['igvmSha256','launchMeasurement','profileSha256']);
const prior=old.images.filter(x=>x.nativeDesktop);assert.equal(prior.length,1);assert.equal(prior[0].minimumGuestSvn,24);assert.equal(prior[0].igvmSha256,OLD_IGVM);
const image={...prior[0],measurement:release.launchMeasurement,igvmSha256:release.igvmSha256,profileSha256:c.profileSha256,controllerSha256:c.controllerSha256,identity:c.imageIdentity,minimumGuestSvn:25};
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
assert(env.includes(OLD_IGVM)&&env.includes('SVN24'));
env=env.replaceAll(OLD_IGVM,release.igvmSha256).replaceAll(sha(fs.readFileSync(path.join(live,'desktop-config.json'))),sha(files['desktop-config.json'])).replaceAll(sha(oldReleaseRaw),sha(files['desktop-release.json'])).replace(/^rem Native desktop SVN24.*$/m,'rem Native desktop SVN25 (the Shield compute overlay of enclave-apps 0d63827; everything else is the release before it). Not hardware-qualified.');
assert(!env.includes(OLD_IGVM)&&!env.includes('4c8adc8d')&&!env.includes('12f22930')&&env.includes('SVN25')&&!env.includes('SVN24'));
files['native-desktop-env.cmd']=env;
for(const [n,x] of Object.entries(files))fs.writeFileSync(path.join(h,n),x,{flag:'wx'});
const hashes=Object.fromEntries(Object.entries(files).map(([n,x])=>[n,sha(x)]));
const stage=Object.fromEntries(['native-igvm.bin','root.iso','hashes.iso','profile.json'].map(n=>[n,{bytes:c.files[n].bytes,sha256:c.files[n].sha256}]));
for(const n of ['desktop-config.json','desktop-release.json','native-desktop-env.cmd','shield-policy.json'])stage[n]={bytes:Buffer.byteLength(files[n]),sha256:hashes[n]};
stage['shielded-bridge.exe']={copyFrom:oldTarget+'\\shielded-bridge.exe',sha256:c.requiredBridgeSha256};
fs.writeFileSync(path.join(h,'prepared.json'),text({target,previousTarget:oldTarget,guestSvn:25,imageSha256:release.igvmSha256,profileSha256:release.profileSha256,launchMeasurement:release.launchMeasurement,controllerCid:CID,controllerSha256:release.controllerSha256,catalog:{app:APP,version:2,label:'0.9.2'},deployment:DEPLOYMENT,purchased:{cpuMilli:170,gpuMilli:100},appId:next.appId,previousAppId:was.appId,recordSha256:next.recordSha256,ownerSignatureNeeded:false,releaseSchema:release.schema,hostBundle:prev.hostBundle,hostBundleChanges:false,oldPolicySha256:sha(oldPolicyRaw),files:hashes,stage}),{flag:'wx'});
console.log(JSON.stringify({prepared:true,appId:next.appId,previousAppId:was.appId,recordSha256:next.recordSha256,files:hashes}));
