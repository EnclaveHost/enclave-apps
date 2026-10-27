import test from 'node:test';
import http from 'node:http';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import {coordinator,loadPlan,durableWrite,Refusal} from '../core.mjs';
import {createServer} from '../server.mjs';
import {transactionOf,decodeFunctionData,clients,verifyTransaction} from '../chain.mjs';
import {verifyCar,carHeaderOf} from '../upload.mjs';
import {importFile,cidString} from '../unixfs.mjs';
const root=path.resolve(import.meta.dirname,'..');
const wallet='0x1111111111111111111111111111111111111111';
const hash='0x'+'a'.repeat(64),planHash='b'.repeat(64);
function fixture(options={}){
 const actions=[{id:'upload',kind:'upload',status:'ready',gates:['release']},{id:'publish',kind:'publish',status:'ready',gates:['release'],dependsOn:['upload']}];
 const plan={chainId:8453,wallet,status:'ready',gates:[{id:'release',status:'verified'}],actions,...options.plan};
 let saved=[],calls=0;const initial=options.initial||{};
 const queue=coordinator({plan,planHash,initial,save:s=>{if(options.diskFail)throw Error('disk full');saved.push(structuredClone(s));},prepare:async()=>{calls++;return {transaction:{from:wallet,to:wallet,nonce:'0x7',value:'0x0'},expiresAt:Date.now()+60000};},verify:async()=>options.receipt||{status:'confirmed',hash},uploadMessage:()=>({message:'test only',hexMessage:'0x12',expiry:Math.floor(Date.now()/1000)+300}),runUpload:async()=>{if(options.uploadFail)throw Error('SECRET_SIGNATURE');return {cid:'verified'};}});
 const body=id=>({id,account:wallet,chainId:'0x2105'});
 return {queue,body,plan,saved,get calls(){return calls;}};
}
test('current plan pins RISC publication and gates private update on publication and migration',()=>{
 const {plan}=loadPlan(root);assert.equal(plan.status,'ready');assert.deepEqual(plan.actions.map(a=>a.kind),['upload','publish','snapshot-update']);assert.equal(plan.actions[2].nextConfig,undefined);assert.deepEqual(plan.actions[2].dependsOn,['risc-publish-070']);assert(plan.actions.every(a=>a.slug==='risc-box'));assert.deepEqual(plan.actions[1].dependsOn,[plan.actions[0].id]);assert.equal(JSON.parse(plan.actions[1].release.config).snapshot,undefined);assert.deepEqual(plan.apps.map(a=>a.slug).sort(),['kvm-box-native','risc-box']);
 assert(plan.gates.find(g=>g.id==='native-runtime').status==='blocked');
});
test('blocked plan cannot emit a signing request even with an action injected',async()=>{
 const f=fixture({plan:{status:'blocked'}});await assert.rejects(f.queue.prepare(f.body('upload')),/blocked/);assert.equal(f.saved.length,0);
});
test('wallet and network checked before preparing',async()=>{
 const f=fixture();await assert.rejects(f.queue.prepare({...f.body('upload'),account:'0x2222'}),/wallet/);await assert.rejects(f.queue.prepare({...f.body('upload'),chainId:1}),/wallet/);assert.equal(f.saved.length,0);
});
test('dependency gates prevent publish before upload',async()=>{
 const f=fixture();await assert.rejects(f.queue.prepare(f.body('publish')),/preceding/);assert.equal(f.calls,0);
});
test('unverified readiness blocks a ready-looking plan',async()=>{
 const f=fixture({plan:{gates:[{id:'release',status:'blocked'}]}});await assert.rejects(f.queue.prepare(f.body('upload')),/Prerequisite/);
});
test('journal write failure returns no request',async()=>{
 const f=fixture({diskFail:true});await assert.rejects(f.queue.prepare(f.body('upload')),/disk full/);assert.equal(f.queue.view().open,null);
});
test('one active review; stale or non-4001 rejection cannot reset dispatch',async()=>{
 const f=fixture(),s=await f.queue.prepare(f.body('upload'));
 await assert.rejects(f.queue.prepare(f.body('upload')),/open signing/);
 await f.queue.dispatch({...f.body('upload'),attemptId:s.attemptId});
 assert.equal(f.saved.at(-1).actions.upload.status,'awaiting-wallet');
 assert.throws(()=>f.queue.reject({id:'upload',attemptId:'old',code:4001}),/current wallet/);
 assert.throws(()=>f.queue.reject({id:'upload',attemptId:s.attemptId,code:-32000}),/explicit rejection/);
 assert.throws(()=>f.queue.cancel({id:'upload',attemptId:s.attemptId}),/cannot be cancelled/);
 f.queue.reject({id:'upload',attemptId:s.attemptId,code:4001});assert.equal(f.queue.view().open,null);
});
test('upload succeeds without signature in journal; then publish becomes available',async()=>{
 const f=fixture(),s=await f.queue.prepare(f.body('upload'));await f.queue.dispatch({...f.body('upload'),attemptId:s.attemptId});
 await f.queue.result({id:'upload',attemptId:s.attemptId,signature:'SECRET_SIGNATURE'});
 assert(!JSON.stringify(f.saved).includes('SECRET_SIGNATURE'));
 const p=await f.queue.prepare(f.body('publish'));assert.equal(p.status,'review');
});
test('upload failures suppress remote details and signature',async()=>{
 const f=fixture({uploadFail:true}),s=await f.queue.prepare(f.body('upload'));await f.queue.dispatch({...f.body('upload'),attemptId:s.attemptId});
 await assert.rejects(f.queue.result({id:'upload',attemptId:s.attemptId,signature:'SECRET_SIGNATURE'}),/verification failed/);
 assert(!JSON.stringify(f.saved).includes('SECRET_SIGNATURE'));
});
const uploaded={planHash,actions:{upload:{status:'uploaded'}}};
test('submitted hash cannot be replaced; pending request survives restart',async()=>{
 const f=fixture({initial:uploaded,receipt:{status:'pending',hash}}),s=await f.queue.prepare(f.body('publish'));await f.queue.dispatch({...f.body('publish'),attemptId:s.attemptId});
 await f.queue.result({id:'publish',attemptId:s.attemptId,hash});
 await assert.rejects(f.queue.verify({id:'publish',attemptId:s.attemptId,hash:'0x'+'b'.repeat(64)}),/cannot be replaced/);
 const restored=fixture({initial:f.saved.at(-1)});await assert.rejects(restored.queue.prepare(restored.body('publish')),/open signing/);
 await restored.queue.verify({id:'publish',attemptId:s.attemptId,hash});assert.equal(restored.queue.view().actions[1].state.status,'confirmed');
});
test('journal cannot be resumed under another plan',()=>assert.throws(()=>fixture({initial:{planHash:'c'.repeat(64)}}),/different plan/));
test('atomic update changes only appRef and config; native wraps in ownerCall',()=>{
 const plan={wallet,contracts:{deployments:'0x2222222222222222222222222222222222222222'}},id='0x'+'f'.repeat(64);
 const a={kind:'update',deploymentId:id,nextAppRef:'catalog://test/1',nextConfig:'{"config":{"key":"$APP_KEY"}}'};
 const abi=JSON.parse(fs.readFileSync(path.join(root,'abi/deployments.json')));const tx=transactionOf(plan,a),decoded=decodeFunctionData({abi,data:tx.data});assert.equal(decoded.functionName,'multicall');assert.equal(decoded.args[0].length,2);
 assert.deepEqual(decoded.args[0].map(data=>decodeFunctionData({abi,data}).functionName),['setAppRef','setConfig']);
 const wrapped=transactionOf(plan,{...a,vault:'0x3333333333333333333333333333333333333333'});assert.equal(wrapped.to,'0x3333333333333333333333333333333333333333');assert.equal(wrapped.value,'0x0');assert.equal(wrapped.chainId,'0x2105');
});
test('CAR verification rejects missing blocks and corruption',()=>{
 const dag=importFile(Buffer.from('small frozen artifact')),t={expectedCid:cidString(dag.cid)},header=carHeaderOf(t.expectedCid);
 const vi=n=>{const b=[];do{b.push((n%128)+(n>=128?128:0));n=Math.floor(n/128);}while(n);return Buffer.from(b);};
 const blocks=[...dag.blocks].map(([c,b])=>Buffer.concat([vi(c.length+b.length),c,b]));
 const car=Buffer.concat([vi(header.length),header,...blocks]);assert.equal(verifyCar(t,car,dag).blocks,[...dag.blocks].length);
 assert.throws(()=>verifyCar(t,Buffer.concat([vi(header.length),header]),dag),/lacks/);
 const corrupt=Buffer.from(car);corrupt[corrupt.length-1]^=1;assert.throws(()=>verifyCar(t,corrupt,dag),/hash/);
});
test('durable write persists the entire journal privately',()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'sia-rollout-test-'));try{const file=path.join(dir,'journal.json');durableWrite(file,{a:1});assert.deepEqual(JSON.parse(fs.readFileSync(file)),{a:1});assert.equal(fs.statSync(file).mode&0o777,0o600);}finally{fs.rmSync(dir,{recursive:true,force:true});}
});
test('HTTP enforces exact Host, Origin and token; blocked prepare returns no transaction',async()=>{
 const {plan,trust}=loadPlan(root);const queue=coordinator({plan,planHash:trust.planSha256,save:()=>{throw Error('must not write');}});
 const app=createServer({plan,trust,inventory:{},queue,assets:{'/':{body:'test',type:'text/html'}},evidence:{},refresh:async()=>({fresh:true})});
 const url=await app.listen(0);
 try{
  const response=await fetch(url+'/api/state'),s=await response.json();assert.equal(response.status,200);assert.match(response.headers.get('content-security-policy'),/frame-ancestors 'none'/);assert.equal(response.headers.get('cache-control'),'no-store');
  const badHost=await new Promise((resolve,reject)=>{http.get(url+'/api/state',{headers:{Host:'evil.test'}},r=>{r.resume();resolve(r.statusCode);}).on('error',reject);});assert.equal(badHost,403);
  const navigation=await new Promise((resolve,reject)=>{http.get(url+'/',{headers:{'Sec-Fetch-Site':'cross-site','Sec-Fetch-Mode':'navigate'}},r=>{r.resume();resolve(r.statusCode);}).on('error',reject);});assert.equal(navigation,200);
  assert.equal((await fetch(url+'/api/state',{headers:{'Sec-Fetch-Site':'cross-site'}})).status,403);
  assert.equal((await fetch(url+'/api/refresh',{method:'POST',headers:{Origin:'https://evil.test','Content-Type':'application/json'},body:'{}'})).status,403);
  assert.equal((await fetch(url+'/api/refresh',{method:'POST',headers:{Origin:url,'Content-Type':'application/json'},body:'{}'})).status,403);
  const blocked=await fetch(url+'/api/prepare',{method:'POST',headers:{Origin:url,'Content-Type':'application/json','X-Rollout-Token':s.token},body:JSON.stringify({id:'made-up',account:plan.wallet,chainId:8453})});assert.equal(blocked.status,409);assert(!JSON.stringify(await blocked.json()).includes('transaction'));
  const refresh=await fetch(url+'/api/refresh',{method:'POST',headers:{Origin:url,'Content-Type':'application/json','X-Rollout-Token':s.token},body:'{}'});assert.equal(refresh.status,200);
 }finally{await new Promise(resolve=>app.server.close(resolve));}
});

test('receipt verification rejects wallet-altered gas and nonce before confirming',async()=>{
 const originals=[...clients];const tx={from:wallet,to:wallet,data:'0x1234',value:'0x0',gas:'0x123',nonce:'0x7',maxFeePerGas:'0x10',maxPriorityFeePerGas:'0x1',chainId:'0x2105'};
 try{
  for(const field of ['gas','nonce','value','maxFeePerGas','maxPriorityFeePerGas','chainId']){
   const actual={...tx,input:tx.data,[field]:BigInt(tx[field])+1n};const mock={getTransaction:async()=>actual,getTransactionReceipt:async()=>({status:'success',blockNumber:1n,blockHash:hash})};clients.splice(0,2,mock,mock);
   await assert.rejects(verifyTransaction({wallet},{},{transaction:tx},hash),new RegExp(field+' differs'));
  }
 }finally{clients.splice(0,2,...originals);}
});
