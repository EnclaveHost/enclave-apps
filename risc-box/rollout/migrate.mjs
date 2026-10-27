import fs from 'node:fs';import path from 'node:path';import {createHash,createHmac,randomUUID} from 'node:crypto';import {spawn} from 'node:child_process';
import {durableWrite,Refusal,sha} from './core.mjs';
const EMPTY=sha(''),floor=48*1024**3,limit=3*1024**3;
const fail=m=>{throw new Refusal(m);};
export const encodeObject=s=>s.split('/').map(x=>encodeURIComponent(x).replace(/[!'()*]/g,c=>'%'+c.charCodeAt(0).toString(16).toUpperCase())).join('/');
export function signedRequest({endpoint,region,credentials},method,bucket,key,payloadHash=EMPTY,extra={},clock=new Date()){
 const url=new URL(endpoint);if(url.username||url.password||url.search||url.hash||url.pathname!=='/')fail('S3 endpoint must be a plain origin');
 if(url.protocol!=='https:'&&!(url.protocol==='http:'&&['127.0.0.1','[::1]'].includes(url.hostname)))fail('Migration requires HTTPS S3 (HTTP allowed only for local tests)');
 if(!/^[a-zA-Z0-9._-]+$/.test(bucket)||!key||key.split('/').some(x=>x==='.'||x==='..'))fail('Invalid S3 object path');
 url.pathname='/'+bucket+'/'+encodeObject(key);
 const stamp=clock.toISOString().replace(/[-:]/g,'').replace(/\.\d{3}Z$/,'Z'),date=stamp.slice(0,8),scope=`${date}/${region}/s3/aws4_request`;
 const headers={host:url.host,'x-amz-content-sha256':payloadHash,'x-amz-date':stamp,...extra,...(credentials.sessionToken?{'x-amz-security-token':credentials.sessionToken}:{})};
 const names=Object.keys(headers).sort(),canonical=names.map(k=>`${k}:${String(headers[k]).trim()}\n`).join('');
 const request=`${method}\n${url.pathname}\n\n${canonical}\n${names.join(';')}\n${payloadHash}`;
 const hmac=(key,s)=>createHmac('sha256',key).update(s).digest();
 let keyBytes=Buffer.from('AWS4'+credentials.secretAccessKey);for(const s of [date,region,'s3','aws4_request']){const next=hmac(keyBytes,s);keyBytes.fill(0);keyBytes=next;}
 const signature=hmac(keyBytes,`AWS4-HMAC-SHA256\n${stamp}\n${scope}\n${sha(request)}`).toString('hex');keyBytes.fill(0);
 headers.authorization=`AWS4-HMAC-SHA256 Credential=${credentials.accessKeyId}/${scope}, SignedHeaders=${names.join(';')}, Signature=${signature}`;
 return {url:url.href,headers};
}
async function hashFile(file){const h=createHash('sha256');let bytes=0;for await(const chunk of fs.createReadStream(file)){h.update(chunk);bytes+=chunk.length;}return {sha256:h.digest('hex'),bytes};}
export function snapshotMigration({plan,directory,privateFlow,checkDeployment,tool,toolSha256,fetchFn=fetch,resourceCheck=()=>{const s=fs.statfsSync(directory);if(s.bavail*s.bsize<floor)fail('Local free disk crossed the 48 GiB floor');const a=/MemAvailable:\s+(\d+)/.exec(fs.readFileSync('/proc/meminfo','utf8'));if(!a||Number(a[1])*1024<16*1024**3)fail('Local memory floor crossed');}}){
 const cfg=plan.privateSnapshot,record=path.join(directory,'migration.json');
 const old=JSON.parse(cfg.baseline.configCid),c=old.config;
 if(!c||typeof c.snapshot!=='string'||!c.snapshot||!c.snapshotSaveKey)fail('Legacy snapshot configuration is missing');
 let state=fs.existsSync(record)?JSON.parse(fs.readFileSync(record)):{status:'idle'};
 const save=s=>{durableWrite(record,s);state=s;};
 const resolve=(value,env)=>{if(typeof value!=='string')fail('Missing storage setting');if(value.startsWith('$')){const name=value.slice(1).replace(/^\{(.*)\}$/,'$1');if(!env[name])fail('Required storage secret is missing');return env[name];}return value;};
 async function crypt(mode,object,input,output,context){
  if(sha(fs.readFileSync(tool))!==toolSha256)fail('Migration binary changed');resourceCheck();
  await new Promise((yes,no)=>{
   const child=spawn(tool,[mode,'risc-box',c.bucket,object,input,output],{stdio:['pipe','ignore','pipe'],env:{PATH:'/usr/bin:/bin'},timeout:120000,killSignal:'SIGKILL'});
   let size=0;child.stderr.on('data',b=>{size+=b.length;if(size>8192)child.kill('SIGKILL');});child.on('error',()=>no(new Refusal('Migration tool could not run')));child.on('close',code=>code===0?yes():no(new Refusal('Snapshot encryption/authentication failed')));
   child.stdin.on('error',()=>{});child.stdin.end(JSON.stringify({snapshotEncryption:{masterKey:context.masterKey,owner:context.owner,deployment:context.deployment}}));
  });resourceCheck();
 }
 return {view:()=>state,
 async run(){
  if(state.status==='verified')return state;
  if(state.status!=='idle')fail('An earlier migration needs reconciliation; originals and its receipt are retained');
  if(!privateFlow.view().verified)fail('Verify the provisioned snapshot key with a signed owner read first');
  await checkDeployment(cfg);resourceCheck();
  const context=privateFlow.privateContext(),env=context.env;
  const storage={endpoint:resolve(c.endpoint,env),region:c.region||'auto',credentials:{accessKeyId:resolve(c.credentials.accessKeyId,env),secretAccessKey:resolve(c.credentials.secretAccessKey,env),...(c.credentials.sessionToken?{sessionToken:resolve(c.credentials.sessionToken,env)}:{})}};
  const run=randomUUID(),work=path.join(directory,'migration-'+run);fs.mkdirSync(work,{mode:0o700});
  const objects=[{role:'boot',source:c.snapshot},{role:'saved',source:c.snapshotSaveKey}];
  const receipt={status:'running',run,deployment:cfg.deploymentId,objects:[],startedAt:new Date().toISOString()};save(receipt);
  const request=async(method,key,hash=EMPTY,extra={},body)=>{resourceCheck();const r=signedRequest(storage,method,c.bucket,key,hash,extra);return fetchFn(r.url,{method,headers:r.headers,redirect:'error',signal:AbortSignal.timeout(180000),...(body?{body,duplex:'half'}:{})});};
  async function download(key,file,optional=false){
   const r=await request('GET',key);
   if(optional&&r.status===404){await r.body?.cancel();return null;}
   if(!r.ok){await r.body?.cancel();fail('S3 object read failed; no deployment changes were made');}
   const length=Number(r.headers.get('content-length'));
   if(!Number.isSafeInteger(length)||length<1||length>limit){await r.body?.cancel();fail('Snapshot size is absent or exceeds the migration bound');}
   const stat=fs.statfsSync(directory);if(stat.bavail*stat.bsize<floor+3*length+64*1024**2){await r.body?.cancel();fail('Not enough disk above the migration reserve');}
   const fd=fs.openSync(file,'wx',0o600),hash=createHash('sha256');let bytes=0;
   try{for await(const b of r.body){resourceCheck();bytes+=b.length;if(bytes>length)fail('S3 body exceeded its declared size');hash.update(b);let offset=0;while(offset<b.length)offset+=fs.writeSync(fd,b,offset,b.length-offset);}if(bytes!==length)fail('S3 body was truncated');fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
   return {bytes,sha256:hash.digest('hex'),etag:r.headers.get('etag')};
  }
  try{
   for(const item of objects){
    const input=path.join(work,item.role+'.plain'),sealed=path.join(work,item.role+'.sealed'),verified=path.join(work,item.role+'.verified');
    const source=await download(item.source,input,item.role==='saved');
    if(!source){receipt.objects.push({...item,missing:true});save(receipt);continue;}
    const fd=fs.openSync(input,'r'),magic=Buffer.alloc(12);try{fs.readSync(fd,magic,0,12,0);}finally{fs.closeSync(fd);}
    if(magic.subarray(0,8).toString('binary')!=='RBXSNAP\0'||magic.readUInt32LE(8)!==1)fail('Legacy object is not a supported RISC snapshot');
    const target=`private/${cfg.deploymentId}/migration-${run}/${item.role}.snap`;
    const out={...item,target,original:source,stage:'encrypting'};receipt.objects.push(out);save(receipt);
    await crypt('seal',target,input,sealed,context);await crypt('open',target,sealed,verified,context);
    if((await hashFile(verified)).sha256!==source.sha256)fail('Local encryption round-trip differs');
    fs.unlinkSync(verified);fs.unlinkSync(input);out.ciphertext=await hashFile(sealed);out.stage='uploading';save(receipt);
    await checkDeployment(cfg);
    const head=await request('HEAD',target);await head.body?.cancel();if(head.status!==404)fail('Migration destination is not confirmed absent');
    const put=await request('PUT',target,out.ciphertext.sha256,{'if-none-match':'*','content-type':'application/octet-stream','content-length':String(out.ciphertext.bytes)},fs.createReadStream(sealed));
    await put.body?.cancel();if(!put.ok)fail('Encrypted S3 write was not confirmed');
    const returned=path.join(work,item.role+'.returned');const remote=await download(target,returned);
    if(remote.sha256!==out.ciphertext.sha256)fail('Stored ciphertext differs from the uploaded snapshot');
    await crypt('open',target,returned,verified,context);if((await hashFile(verified)).sha256!==source.sha256)fail('Stored snapshot does not decrypt to the original');
    fs.unlinkSync(verified);fs.unlinkSync(returned);fs.unlinkSync(sealed);out.stage='verified';save(receipt);
   }
   await checkDeployment(cfg);receipt.status='verified';receipt.finishedAt=new Date().toISOString();save(receipt);return state;
  }catch(e){for(const item of objects)for(const suffix of ['.plain','.verified']){const f=path.join(work,item.role+suffix);if(fs.existsSync(f))fs.unlinkSync(f);}
   receipt.status='held';receipt.error=e instanceof Refusal?e.message:'Migration failed; reconcile retained objects before retrying';save(receipt);throw new Refusal(receipt.error);}
 }
 };
}
