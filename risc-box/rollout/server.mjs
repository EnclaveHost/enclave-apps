import http from 'node:http';
import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {randomBytes} from 'node:crypto';
import {loadPlan,coordinator,durableWrite,Refusal,sha} from './core.mjs';
import {snapshot,prepareTransaction,verifyTransaction,checkPrivateDeployment} from './chain.mjs';
import {uploadMessage,runUpload,frozenWasm} from './upload.mjs';
import {snapshotMigration} from './migrate.mjs';
import {privateUpdateAction} from './update.mjs';
import {privateProvisioning} from './private.mjs';
const root=path.dirname(fileURLToPath(import.meta.url));
export function createServer({plan,trust,inventory,queue,refresh,assets,evidence,privateFlow,migration}){
 const token=randomBytes(32).toString('hex');let busy=false,origin,refreshError=null;
 const reply=(res,status,data,type='application/json')=>{res.writeHead(status,{'Content-Type':type,'Cache-Control':'no-store','X-Content-Type-Options':'nosniff','Referrer-Policy':'no-referrer','Content-Security-Policy':"default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self' data:; base-uri 'none'; frame-ancestors 'none'; form-action 'none'"});res.end(type==='application/json'?JSON.stringify(data):data);};
 const state=()=>({token,plan,inventory,refreshError,queue:queue.view(),private:privateFlow?.view()||null,migration:migration?.view()||null,serverTime:new Date().toISOString()});
 const server=http.createServer(async(req,res)=>{
  if(req.headers.host!==new URL(origin).host)return reply(res,403,{error:'Invalid Host'});
  if(req.headers.origin&&req.headers.origin!==origin)return reply(res,403,{error:'Cross-origin request refused'});
  if(req.headers['sec-fetch-site']==='cross-site'&&!(req.method==='GET'&&req.url==='/'&&req.headers['sec-fetch-mode']==='navigate'))return reply(res,403,{error:'Cross-site request refused'});
  const url=new URL(req.url,origin),route=url.pathname;
  if(req.method==='GET'){
   if(route==='/api/state')return reply(res,200,state());
   if(route==='/api/report')return reply(res,200,{plan,planSha256:trust.planSha256,inventory,signing:queue.view(),releaseEvidence:evidence,exportedAt:new Date().toISOString()});
   const asset=assets[route];if(asset)return reply(res,200,asset.body,asset.type);
   return reply(res,404,{error:'Not found'});
  }
  if(req.method!=='POST')return reply(res,405,{error:'Method not allowed'});
  if(req.headers.origin!==origin||req.headers['x-rollout-token']!==token||!/^application\/json(?:;|$)/i.test(req.headers['content-type']||''))return reply(res,403,{error:'Local session required'});
  if(busy)return reply(res,409,{error:'Another action is in progress'});busy=true;
  try{
   let raw='',size=0;for await(const chunk of req){size+=chunk.length;if(size>8192)throw new Refusal('Request too large');raw+=chunk;}
   let body;try{body=JSON.parse(raw||'{}');}catch{throw new Refusal('Invalid JSON');}
   if(route==='/api/refresh'){
    try{inventory=await refresh();refreshError=null;}catch{refreshError='Live chain refresh failed. The last successful snapshot remains visible; do not treat it as current.';return reply(res,503,{error:refreshError});}
    return reply(res,200,state());
   }
   if(route==='/api/migrate'){
    if(!migration)throw new Refusal('Migration is not configured');
    if(queue.view().open||['review','awaiting-wallet','submitting'].includes(privateFlow.view().status))throw new Refusal('Resolve the current wallet request first');
    if(String(body.account).toLowerCase()!==plan.wallet.toLowerCase()||Number(body.chainId)!==plan.chainId)throw new Refusal('Connect the reviewed wallet on Base');
    return reply(res,200,await migration.run());
   }
   if(route.startsWith('/api/private/')){
    if(!privateFlow)throw new Refusal('Private provisioning is not configured');
    if(queue.view().open)throw new Refusal('Resolve the publication wallet request first');
    const method=route.slice('/api/private/'.length);
    if(!['prepare','dispatch','cancel','reject','submit'].includes(method))throw new Refusal('Unknown private operation');
    return reply(res,200,await privateFlow[method](body));
   }
   if(privateFlow&&['review','awaiting-wallet','submitting'].includes(privateFlow.view().status))throw new Refusal('Resolve the private provisioning request first');
   const name=route.replace('/api/','');if(!['prepare','dispatch','cancel','reject','result','verify'].includes(name))return reply(res,404,{error:'Unknown action'});
   const result=await queue[name](body);return reply(res,200,result);
  }catch(e){return reply(res,e instanceof Refusal?409:503,{error:e instanceof Refusal?e.message:'The operation could not be verified. No automatic retry was made. Recheck the journal or RPC state.'});}
  finally{busy=false;}
 });
 return {server,listen(port=trust.port){return new Promise((resolve,reject)=>{server.once('error',reject);server.listen(port,'127.0.0.1',()=>{origin='http://127.0.0.1:'+server.address().port;resolve(origin);});});}};
}
async function main(){
 const {plan,trust}=loadPlan(root);
 const data=process.env.SIA_ROLLOUT_STATE_DIR||path.resolve(root,'../../work/sia-rollout-state');fs.mkdirSync(data,{recursive:true,mode:0o700});
 const journal=path.join(data,'journal.json');const initial=fs.existsSync(journal)?JSON.parse(fs.readFileSync(journal)):{};
 const uploadTrust=a=>{
  const artifact=path.resolve(root,a.artifact.file);if(!artifact.startsWith(root+path.sep))throw new Refusal('Artifact path is outside the release directory');
  return {admin:plan.wallet,wasm:{path:artifact,sha256:a.artifact.sha256,bytes:a.artifact.bytes},expectedCid:a.artifact.cid,upload:{tokenUrl:'https://api.enclave.host/v1/apps/upload-token',addUrl:'https://ipfs.enclave.host/add-wasm',carUrl:cid=>'https://ipfs.enclave.host/ipfs/'+cid+'?format=car&dag-scope=all',expirySec:300,signMarginSec:20,maxJsonBytes:65536,maxCarBytes:a.artifact.bytes+Math.ceil(a.artifact.bytes/262144)*128+2*1024*1024,timeoutMs:600000}};
 };
 let privateFlow,migration;
 const resolveAction=a=>a.kind==='snapshot-update'?privateUpdateAction(plan,migration?.view(),privateFlow?.view()):a;
 const queue=coordinator({plan,planHash:trust.planSha256,initial,save:s=>durableWrite(journal,s),prepare:async a=>{const frozen=resolveAction(a);return {...await prepareTransaction(plan,frozen),...(a.kind==='snapshot-update'?{frozenAction:frozen}:{})};},verify:(a,r,h)=>verifyTransaction(plan,r.frozenAction||a,r,h),uploadMessage:(a,now)=>{const t=uploadTrust(a);frozenWasm(t);return uploadMessage(t,now);},runUpload:(a,auth,signature)=>runUpload({t:uploadTrust(a),auth,signature,fetchFn:fetch})});
 const assets=Object.fromEntries([['/','index.html','text/html'],['/app.js','app.js','text/javascript'],['/style.css','style.css','text/css']].map(([url,file,type])=>[url,{body:fs.readFileSync(path.join(root,'dist',file)),type}]));
 const last=path.join(data,'inventory.json');const inventory=JSON.parse(fs.readFileSync(fs.existsSync(last)?last:path.join(root,'inventory.json')));
 privateFlow=privateProvisioning({plan,directory:path.join(data,'private'),checkDeployment:cfg=>checkPrivateDeployment(plan,cfg)});
 migration=plan.privateSnapshot?snapshotMigration({plan,directory:path.join(data,'private'),privateFlow,checkDeployment:cfg=>checkPrivateDeployment(plan,cfg),tool:process.env.SNAPSHOT_CRYPTO_TOOL||path.resolve(root,'../../work/snapshot-target/debug/snapshot-crypto'),toolSha256:'cb1d5deeb91e6beaf17baefe5cc935f248a857d4af81a2ef3020f484f0a0a8b8'}):null;
 const app=createServer({privateFlow,migration,plan,trust,inventory,queue,assets,evidence:plan.evidence.map(e=>({...e,content:JSON.parse(fs.readFileSync(path.resolve(root,e.file)))})),refresh:async()=>{const v=await snapshot(plan.apps.flatMap(a=>a.deploymentIds));durableWrite(last,v);return v;}});
 console.log('Sia rollout page: '+await app.listen());console.log('Plan '+trust.planSha256+' ('+plan.status+'); no wallet or storage keys loaded.');
}
if(process.argv[1]&&path.resolve(process.argv[1])===fileURLToPath(import.meta.url))main().catch(e=>{console.error(e instanceof Refusal?e.message:'Startup refused. Check the local plan, journal and port.');process.exitCode=1;});
