import fs from 'node:fs';
import path from 'node:path';
import {randomBytes,randomUUID} from 'node:crypto';
import {recoverMessageAddress,stringToHex} from './vendor/viem.mjs';
import {durableWrite,Refusal,sha} from './core.mjs';
const refuse=m=>{throw new Refusal(m);};
const KEY='RISCBOX_SNAPSHOT_KEY',OWNER='RISCBOX_SNAPSHOT_OWNER',DEPLOY='RISCBOX_SNAPSHOT_DEPLOYMENT';
const validKey=s=>typeof s==='string'&&/^[0-9a-f]{64}$/i.test(s)&&!/^0{64}$/.test(s);
export function privateProvisioning({plan,directory,checkDeployment,fetchFn=fetch,recover=recoverMessageAddress,now=Date.now}){
 const cfg=plan.privateSnapshot;if(!cfg)return null;
 const id=cfg.deploymentId.toLowerCase(),owner=plan.wallet.toLowerCase();
 if(!/^0x[0-9a-f]{64}$/.test(id)||!/^0x[0-9a-f]{40}$/.test(owner))refuse('Invalid private snapshot binding');
 fs.mkdirSync(directory,{recursive:true,mode:0o700});
 const st=fs.lstatSync(directory);if(!st.isDirectory()||st.isSymbolicLink()||(st.mode&0o077))refuse('Private state directory permissions are unsafe');
 const file=path.join(directory,'snapshot-key.json'),journal=path.join(directory,'provisioning.json');
 function readPrivate(f){if(!fs.existsSync(f))return null;const s=fs.lstatSync(f);if(!s.isFile()||s.isSymbolicLink()||(s.mode&0o077))refuse('Private state file permissions are unsafe');return JSON.parse(fs.readFileSync(f));}
 let state=readPrivate(journal)||{status:'idle'},saved=readPrivate(file),env=null;
 if(saved&&(saved.owner!==owner||saved.deployment!==id||!validKey(saved.masterKey)))refuse('Private key belongs to a different deployment');
 const persist=s=>{durableWrite(journal,s);state=s;};
 if(state.status==='submitting'){state={status:state.operation==='write'?'unknown':'idle',operation:state.operation};durableWrite(journal,state);}
 const publicState=()=>({status:state.status,operation:state.operation||null,attemptId:state.attemptId||null,request:state.request||null,ready:!!env,keyPrepared:!!saved,verified:state.status==='verified',receipt:state.receipt||null,needsOwnerRead:!env,deploymentId:id,owner});
 const account=b=>{if(String(b.account).toLowerCase()!==owner||Number(b.chainId)!==8453)refuse('Connect the reviewed owner wallet on Base');};
 async function relay(s,signature,payload){
  const url='https://api.enclave.host/v1/secrets/'+id+(s.operation==='read'?'/get':'');
  const response=await fetchFn(url,{method:'POST',redirect:'error',headers:{'Content-Type':'application/json'},body:JSON.stringify({expiry:s.request.expiry,signature,...(payload?{payload}:{})}),signal:AbortSignal.timeout(40000)});
  if(!response.ok){await response.body?.cancel();refuse('Owner secret operation refused by the relay; no fallback was attempted');}
  let bytes=0,raw='';for await(const b of response.body){bytes+=b.length;if(bytes>65536)refuse('Secret response exceeded its bound');raw+=Buffer.from(b).toString('utf8');}
  let out;try{out=JSON.parse(raw);}catch{refuse('Invalid secret response');}
  if(out.id?.toLowerCase()!==id)refuse('Secret response deployment mismatch');return out;
 }
 function adopt(out){
  if(!out.env||typeof out.env!=='object'||Array.isArray(out.env))refuse('Missing owner secret map');
  const e=out.env;
  if(e[KEY]){
   if(!validKey(e[KEY])||(e[OWNER]&&e[OWNER].toLowerCase()!==owner)||(e[DEPLOY]&&e[DEPLOY].toLowerCase()!==id))refuse('Existing snapshot secret has an incompatible binding');
   if(saved&&saved.masterKey.toLowerCase()!==e[KEY].toLowerCase())refuse('Relay snapshot key differs from the retained local key; do not rotate or overwrite');
   saved={owner,deployment:id,masterKey:e[KEY].toLowerCase()};
  }else if(!saved)saved={owner,deployment:id,masterKey:randomBytes(32).toString('hex')};
  durableWrite(file,saved);env=e;
  return {rev:out.rev,names:Object.keys(e).sort(),hasMatchingKey:e[KEY]?.toLowerCase()===saved.masterKey,at:new Date(now()).toISOString()};
 }
 return {
  view:publicState,
  privateContext(){if(!env||!saved)refuse('Read the deployment secrets with the owner wallet first');return {env,masterKey:saved.masterKey,owner,deployment:id};},
  async prepare(b){account(b);if(!['read','write','verify'].includes(b.operation))refuse('Unknown private operation');
   if(['review','awaiting-wallet','submitting'].includes(state.status))refuse('Resolve the current private signing request first');
   if(state.status==='unknown'&&b.operation!=='verify')refuse('Read back the uncertain secret merge before any retry');
   if(b.operation==='write'&&(!env||!saved))refuse('Owner read is required before provisioning');
   await checkDeployment(cfg);
   const operation=b.operation==='write'?'write':'read',expiry=Math.floor(now()/1000)+540;
   const payload=operation==='write'?JSON.stringify({set:{[KEY]:saved.masterKey,[OWNER]:owner,[DEPLOY]:id}}):null;
   const message=operation==='write'?`enclave-secrets:put:${id}:${expiry}:${sha(payload)}`:`enclave-secrets:get:${id}:${expiry}`;
   const request={message,hexMessage:stringToHex(message),expiry,deployment:id,purpose:operation==='write'?'Merge only the snapshot key and owner/deployment binding into the existing relay secret store. Other keys are preserved.':'Read this deployment’s existing secret map into the local migration process. Secret values are never returned to the page.',...(payload?{payloadSha256:sha(payload),setNames:[KEY,OWNER,DEPLOY]}:{})};
   persist({status:'review',operation,verification:b.operation==='verify',attemptId:randomUUID(),request});return publicState();
  },
  async dispatch(b){account(b);if(state.status!=='review'||b.attemptId!==state.attemptId)refuse('Private request is not current');if(now()/1000>state.request.expiry-20)refuse('Private authorization expired');await checkDeployment(cfg);persist({...state,status:'awaiting-wallet'});return publicState();},
  cancel(b){if(state.status!=='review'||b.attemptId!==state.attemptId)refuse('Dispatched private request cannot be cancelled');persist({status:'idle'});return publicState();},
  reject(b){if(state.status!=='awaiting-wallet'||b.attemptId!==state.attemptId||b.code!==4001)refuse('Only explicit wallet rejection resets a request');persist({status:'idle'});return publicState();},
  async submit(b){account(b);const s=state;if(s.status!=='awaiting-wallet'||b.attemptId!==s.attemptId)refuse('No matching private wallet request');
   if(now()/1000>s.request.expiry-5||!/^0x[0-9a-f]{130}$/i.test(b.signature||''))refuse('Invalid or expired owner signature');
   if((await recover({message:s.request.message,signature:b.signature})).toLowerCase()!==owner)refuse('Owner signature mismatch');
   await checkDeployment(cfg);
   const payload=s.operation==='write'?JSON.stringify({set:{[KEY]:saved.masterKey,[OWNER]:owner,[DEPLOY]:id}}):null;
   if(payload&&sha(payload)!==s.request.payloadSha256)refuse('Private payload changed');
   persist({...s,status:'submitting'});
   try{
    const out=await relay(s,b.signature,payload);
    if(s.operation==='read'){
     const receipt=adopt(out);
     const verified=receipt.hasMatchingKey&&out.env[OWNER]===owner&&out.env[DEPLOY]===id;
     persist({status:verified?'verified':'read',receipt});
    }else{
     if(![KEY,OWNER,DEPLOY].every(n=>out.names?.includes(n)))refuse('Secret merge response is incomplete');
     persist({status:'merged',receipt:{rev:out.rev,names:[KEY,OWNER,DEPLOY],at:new Date(now()).toISOString()}});
    }
    return publicState();
   }catch{
    persist({status:s.operation==='write'?'unknown':'idle',operation:s.operation});
    refuse(s.operation==='write'?'Secret merge outcome is unknown. Use owner read-back; do not repeat the merge.':'Owner read was not verified. Prepare a fresh read; no secret mutation was requested.');
   }
  }
 };
}
