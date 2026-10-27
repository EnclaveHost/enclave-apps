import {createHash,randomUUID} from 'node:crypto';
import fs from 'node:fs';
import path from 'node:path';
export const sha=b=>createHash('sha256').update(b).digest('hex');
export class Refusal extends Error{}
const fail=m=>{throw new Refusal(m);};
export function durableWrite(file,data){
 fs.mkdirSync(path.dirname(file),{recursive:true,mode:0o700});
 const temp=file+'.'+randomUUID()+'.tmp';
 const fd=fs.openSync(temp,'wx',0o600);
 try{fs.writeFileSync(fd,JSON.stringify(data,null,2)+'\n');fs.fsyncSync(fd);}finally{fs.closeSync(fd);}
 fs.renameSync(temp,file);const dir=fs.openSync(path.dirname(file),'r');try{fs.fsyncSync(dir);}finally{fs.closeSync(dir);}
}
export function loadPlan(root){
 const trust=JSON.parse(fs.readFileSync(path.join(root,'trust.json'))),raw=fs.readFileSync(path.join(root,'plan.json'));
 if(sha(raw)!==trust.planSha256)fail('Plan changed; regenerate and review the pinned handoff before restarting.');
 const plan=JSON.parse(raw);if(plan.schema!=='sia-app-owned-rollout/v1'||plan.chainId!==8453)fail('Unsupported plan');
 if(new Set(plan.actions.map(a=>a.id)).size!==plan.actions.length)fail('Duplicate action IDs');
 for(const e of plan.evidence){const filename=path.resolve(root,e.file);if(!filename.startsWith(root+path.sep)||sha(fs.readFileSync(filename))!==e.sha256)fail('Readiness evidence changed');}
 return {plan,trust};
}
export function coordinator({plan,planHash,initial={},save,prepare,verify,uploadMessage,runUpload,now=Date.now}){
 if(initial.planHash&&initial.planHash!==planHash)fail('Journal belongs to a different plan');
 let journal={planHash,actions:{},...initial};
 const set=(id,value)=>{const next={...journal,updatedAt:new Date(now()).toISOString(),actions:{...journal.actions,[id]:value}};save(next);journal=next;};
 const item=id=>plan.actions.find(a=>a.id===id)||fail('No reviewed signing action exists for this release.');
 const state=id=>journal.actions[id]||{status:'ready'};
 const hasOpen=()=>Object.entries(journal.actions).find(([,s])=>['review','awaiting-wallet','submitted','uploading','halted','reverted'].includes(s.status));
 function eligible(a){
  if(plan.status!=='ready')fail('Release is blocked. Finish the listed prerequisites and freeze a new reviewed plan before signing.');
  if(a.status!=='ready')fail('This action is not ready');
  for(const id of a.gates||[]){const g=plan.gates.find(g=>g.id===id);if(!g||g.status!=='verified')fail('Prerequisite is not verified: '+id);}
  for(const id of a.dependsOn||[])if(!['confirmed','uploaded'].includes(state(id).status))fail('Complete the preceding step first: '+id);
 }
 const account=b=>{if(String(b.account).toLowerCase()!==plan.wallet.toLowerCase()||Number(b.chainId)!==plan.chainId)fail('Connect the reviewed wallet on Base.');};
 const attempt=(s,b)=>{if(!b.attemptId||b.attemptId!==s.attemptId)fail('This is not the current wallet attempt.');};
 return {
  view:()=>({planHash,status:plan.status,actions:plan.actions.map(a=>({...a,state:state(a.id)})),open:hasOpen()?.[0]||null}),
  async prepare(b){account(b);const a=item(b.id);eligible(a);if(hasOpen())fail('Resolve the open signing attempt first.');if(state(a.id).status!=='ready')fail('This action is already complete.');
   const request=a.kind==='upload'?uploadMessage(a,now()):await prepare(a);
   const s={status:'review',attemptId:randomUUID(),request,createdAt:new Date(now()).toISOString()};set(a.id,s);return s;
  },
  async dispatch(b){account(b);const a=item(b.id),s=state(a.id);eligible(a);attempt(s,b);if(s.status!=='review')fail('The signing request is already open or was not reviewed.');
   if(a.kind==='upload'){if(now()/1000>s.request.expiry-20)fail('Upload authorization expired; cancel and prepare again.');}
   else{if(now()>s.request.expiresAt)fail('Transaction preview expired; cancel and prepare again.');const fresh=await prepare(a);if(JSON.stringify(fresh.transaction)!==JSON.stringify(s.request.transaction))fail('Transaction changed since review; cancel and prepare again.');}
   const v={...s,status:'awaiting-wallet'};set(a.id,v);return v;
  },
  cancel(b){const a=item(b.id),s=state(a.id);attempt(s,b);if(s.status!=='review')fail('A dispatched request cannot be cancelled here. Recover its transaction hash.');set(a.id,{status:'ready'});return this.view();},
  reject(b){const a=item(b.id),s=state(a.id);attempt(s,b);if(s.status!=='awaiting-wallet'||b.code!==4001)fail('Only an explicit rejection of the current wallet prompt can reset it.');set(a.id,{status:'ready'});return this.view();},
  async result(b){const a=item(b.id),s=state(a.id);attempt(s,b);if(s.status!=='awaiting-wallet')fail('No wallet result is expected.');
   if(a.kind==='upload'){
    set(a.id,{...s,status:'uploading'});
    try{const receipt=await runUpload(a,s.request,b.signature);set(a.id,{status:'uploaded',receipt});}
    catch{set(a.id,{status:'ready',lastError:'Upload could not be verified. The authorization was not saved. Prepare a new request.'});fail('Upload verification failed. Prepare a new request.');}
    return this.view();
   }
   return this.verify({...b,hash:b.hash});
  },
  async verify(b){const a=item(b.id),s=state(a.id);if(a.kind==='upload')fail('Transaction hashes apply only to on-chain actions.');attempt(s,b);
   if(!['awaiting-wallet','submitted'].includes(s.status))fail('No unresolved transaction is present.');
   if(!/^0x[0-9a-f]{64}$/i.test(b.hash||''))fail('Enter a full transaction hash.');if(s.hash&&s.hash.toLowerCase()!==b.hash.toLowerCase())fail('A submitted hash cannot be replaced.');
   // Persist the supplied hash before network checks; unknown or altered transactions remain held, never retried.
   const pending={...s,hash:b.hash,status:'submitted'};set(a.id,pending);
   const receipt=await verify(a,s.request,b.hash);set(a.id,{...pending,status:receipt.status==='pending'?'submitted':receipt.status,receipt});return this.view();
  }
 };
}
