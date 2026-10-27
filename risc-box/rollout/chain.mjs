import {createPublicClient,http,decodeFunctionData,encodeFunctionData,stringToHex,parseAbi,keccak256,serializeTransaction,decodeFunctionResult} from './vendor/viem.mjs';
import fs from 'node:fs';
export const CHAIN=8453;
export const OWNER='0x0b2d009c0c9af05b12100d77f3c815fea822ee61';
export const BOOK='0xab214342d5A490150A4A977063A2f88E21F80907';
export const SLUGS=['sia-s3','sia-gateway','risc-box','jot','s3-ipfs-adapter','depot','kvm-box-native','ipns-publisher'];
export const clients=['https://base-rpc.publicnode.com','https://mainnet.base.org'].map(url=>createPublicClient({transport:http(url,{timeout:15000,retryCount:1})}));
const ABI_DIR=new URL('./abi/',import.meta.url);
export const abis=Object.fromEntries(['catalog','deployments','book'].map(n=>[n,JSON.parse(fs.readFileSync(new URL(n+'.json',ABI_DIR)))]));
export const stringify=o=>JSON.stringify(o,(_,v)=>typeof v==='bigint'?v.toString():v,2);
export async function addresses(client=clients[0],blockNumber){
 if(await client.getChainId()!==CHAIN)throw Error('RPC is on the wrong chain');
 const [keys,values]=await client.readContract({address:BOOK,abi:abis.book,functionName:'all',blockNumber});
 const result={};for(const [i,k] of keys.entries()){const name=Buffer.from(k.slice(2),'hex').toString().replace(/\0+$/,'');if(['appCatalog','deployments'].includes(name))result[name]=values[i];}
 for(const k of ['appCatalog','deployments'])if(!/^0x[0-9a-f]{40}$/i.test(result[k]||'')||/^0x0{40}$/i.test(result[k]))throw Error('Address book is incomplete');
 return result;
}
export async function snapshot(selectedDeploymentIds=[]){
 const client=clients[0],blockNumber=await client.getBlockNumber(),contracts=await addresses(client,blockNumber);
 const read=(kind,functionName,args=[])=>client.readContract({address:kind==='catalog'?contracts.appCatalog:contracts.deployments,abi:abis[kind],functionName,args,blockNumber});
 let catalog=[];for(let start=0;start<5000;start+=50){const page=await read('catalog','getAppsPage',[BigInt(start),50n]);catalog.push(...page);if(page.length<50)break;}
 catalog=catalog.filter(a=>SLUGS.includes(a.slug));
 const apps=await Promise.all(catalog.map(async a=>({...a,latestIndex:Number(a.versionCount)-1,latest:Number(a.versionCount)?await read('catalog','getVersion',[a.appId,BigInt(a.versionCount)-1n]):null})));
 const ids=new Map(apps.map(a=>[a.appId.toLowerCase(),a]));let deployments=[];
 for(let start=0;start<10000;start+=50){const page=await read('deployments','getPage',[BigInt(start),50n]);
  for(const d of page){const m=/^catalog:\/\/(0x[0-9a-f]{64})\/(\d+)$/i.exec(d.appRef);if(m&&ids.has(m[1].toLowerCase())&&(d.active||selectedDeploymentIds.includes(d.id))){const app=ids.get(m[1].toLowerCase());deployments.push({...d,slug:app.slug,versionIndex:Number(m[2]),version:await read('catalog','getVersion',[app.appId,BigInt(m[2])])});}}
  if(page.length<50)break;
 }
 return JSON.parse(stringify({checkedAt:new Date().toISOString(),chainId:CHAIN,blockNumber,contracts,apps,deployments}));
}

export const vaultAbi=parseAbi(['function owner() view returns (address)','function factory() view returns (address)','function held(bytes32) view returns (uint8,bytes32,bytes32)','function ownerCall(address target, bytes data) returns (bytes)']);
abis.vault=vaultAbi;
export {keccak256,encodeFunctionData,decodeFunctionData};
export const hex=n=>'0x'+BigInt(n).toString(16);
export const norm=x=>JSON.parse(stringify(x));
const same=(a,b,label)=>{if(JSON.stringify(norm(a))!==JSON.stringify(norm(b)))throw Error(label);};
const eq=(a,b,label)=>{if(String(a).toLowerCase()!==String(b).toLowerCase())throw Error(label);};
function callOf(plan,a){
 let to,data;
 if(a.kind==='publish'){
  const r=a.release;to=plan.contracts.appCatalog;
  data=encodeFunctionData({abi:abis.catalog,functionName:'publishVersion',args:[a.slug,r.name,r.description,r.version,r.cid,r.resources,r.ports,r.config,BigInt(r.feePerSec6)]});
 }else if(a.kind==='update'){
  to=plan.contracts.deployments;
  const calls=[encodeFunctionData({abi:abis.deployments,functionName:'setAppRef',args:[a.deploymentId,a.nextAppRef]})];
  if(a.nextConfig!==undefined)calls.push(encodeFunctionData({abi:abis.deployments,functionName:'setConfig',args:[a.deploymentId,a.nextConfig]}));
  data=calls.length===1?calls[0]:encodeFunctionData({abi:abis.deployments,functionName:'multicall',args:[calls]});
 }else if(a.kind==='create'){
  to=plan.contracts.deployments;data=encodeFunctionData({abi:abis.deployments,functionName:'create',args:a.createArgs});
 }else throw Error('Unknown transaction action');
 if(a.vault){data=encodeFunctionData({abi:vaultAbi,functionName:'ownerCall',args:[to,data]});to=a.vault;}
 return {from:plan.wallet,to,data,value:'0x0',chainId:hex(CHAIN)};
}
export function transactionOf(plan,a){return callOf(plan,a);}
function conditionCall(plan,condition){
 const address=condition.contract==='vault'?condition.address:plan.contracts[condition.contract];
 const kind={appCatalog:'catalog',deployments:'deployments',vault:'vault'}[condition.contract];
 if(!kind||!address)throw Error('Unknown condition target');
 const item=abis[kind].find(f=>f.type==='function'&&f.name===condition.fn&&['view','pure'].includes(f.stateMutability));
 if(!item)throw Error('Condition must be a view call');
 return {address,abi:abis[kind],functionName:condition.fn,args:condition.args||[]};
}
function checkCondition(value,c){
 if(c.fields){for(const [k,v] of Object.entries(c.fields))same(value[k],v,'State changed: '+c.fn+'.'+k);}
 else same(value,c.expected,'State changed: '+c.fn);
}
async function conditions(client,plan,list,blockNumber){
 for(const c of list){const v=await client.readContract({...conditionCall(plan,c),blockNumber});checkCondition(v,c);}
}
async function anchor(){
 const heads=await Promise.all(clients.map(c=>c.getBlockNumber({cacheTime:0})));
 const number=heads.reduce((a,b)=>a<b?a:b)-2n;
 const blocks=await Promise.all(clients.map(c=>c.getBlock({blockNumber:number})));
 eq(blocks[0].hash,blocks[1].hash,'RPCs disagree on the anchor');return blocks[0];
}
async function pins(c,plan,a,blockNumber){
 const addr=await addresses(c,blockNumber);
 for(const name of ['appCatalog','deployments'])eq(addr[name],plan.contracts[name],'Address book changed');
 const required=[BOOK,plan.contracts.appCatalog,plan.contracts.deployments,...(a.vault?[a.vault]:[])];
 for(const address of required){
  const expected=plan.codeHashes?.[address.toLowerCase()];if(!expected)throw Error('Missing contract code pin');
  const code=await c.getCode({address,blockNumber});if(!code||code==='0x')throw Error('Missing contract code');eq(keccak256(code),expected,'Contract code changed');
 }
 const code=await c.getCode({address:plan.wallet,blockNumber});if(code&&code!=='0x')throw Error('Wallet account has code; requires a separate review');
 if(a.vault){eq(await c.readContract({address:a.vault,abi:vaultAbi,functionName:'owner',blockNumber}),plan.wallet,'Vault owner changed');}
}
export async function prepareTransaction(plan,a){
 if(!a.preconditions?.length||!a.postconditions?.length)throw Error('Missing pre/post-state pins');
 const started=Date.now(),block=await anchor(),tx=callOf(plan,a);
 const maxFee=BigInt(a.maxFeePerGas),tip=BigInt(a.maxPriorityFeePerGas),gas=BigInt(a.gasCap);
 if(maxFee<=0n||tip<0n||tip>maxFee||gas<=0n)throw Error('Invalid reviewed fee caps');
 if(block.baseFeePerGas*2n+tip>maxFee)throw Error('Current fees exceed the reviewed cap');
 const views=await Promise.all(clients.map(async c=>{
  await pins(c,plan,a,block.number);await conditions(c,plan,a.preconditions,block.number);
  const nonce=await c.getTransactionCount({address:plan.wallet,blockNumber:block.number}),pending=await c.getTransactionCount({address:plan.wallet,blockTag:'pending'});
  if(nonce!==pending)throw Error('Wallet has pending or recently mined transactions; wait before signing');
  const calls=[{from:tx.from,to:tx.to,data:tx.data,value:'0x0',gas:hex(gas)},...a.postconditions.map(q=>{const p=conditionCall(plan,q);return {from:tx.from,to:p.address,data:encodeFunctionData({abi:p.abi,functionName:p.functionName,args:p.args}),gas:'0x1e8480'};})];
  const results=(await c.request({method:'eth_simulateV1',params:[{blockStateCalls:[{calls}],validation:false},hex(block.number)]}))?.[0]?.calls;
  if(!Array.isArray(results)||results.length!==calls.length||results.some(r=>r.status!=='0x1'))throw Error('Simulation failed');
  if(BigInt(results[0].gasUsed)*125n/100n>gas)throw Error('Simulation exceeds the gas cap');
  a.postconditions.forEach((q,i)=>{const p=conditionCall(plan,q);checkCondition(decodeFunctionResult({abi:p.abi,functionName:p.functionName,data:results[i+1].returnData}),q);});
  const raw=serializeTransaction({type:'eip1559',chainId:CHAIN,nonce,to:tx.to,data:tx.data,value:0n,gas,maxFeePerGas:maxFee,maxPriorityFeePerGas:tip});
  const l1=await c.readContract({address:'0x420000000000000000000000000000000000000F',abi:parseAbi(['function getL1Fee(bytes) view returns (uint256)']),functionName:'getL1Fee',args:[raw],blockNumber:block.number});
  if(await c.getBalance({address:plan.wallet,blockNumber:block.number})<gas*maxFee+l1)throw Error('Insufficient ETH for the maximum fee');
  eq((await c.getBlock({blockNumber:block.number})).hash,block.hash,'Anchor changed');return nonce;
 }));
 same(views[0],views[1],'RPCs disagree on nonce');if(Date.now()-started>60000)throw Error('Preparation expired');
 return {transaction:{...tx,nonce:hex(views[0]),gas:hex(gas),maxFeePerGas:hex(maxFee),maxPriorityFeePerGas:hex(tip)},expiresAt:Date.now()+60000,anchorBlock:String(block.number)};
}
export async function verifyTransaction(plan,a,request,hash){
 if(!/^0x[0-9a-f]{64}$/i.test(hash))throw Error('Invalid transaction hash');
 const results=await Promise.all(clients.map(async c=>{
  let actual,receipt;try{actual=await c.getTransaction({hash});receipt=await c.getTransactionReceipt({hash});}catch(e){if(['TransactionNotFoundError','TransactionReceiptNotFoundError'].includes(e.name))return {status:'pending'};throw e;}
  const tx=request.transaction;
  for(const k of ['from','to'])eq(actual[k],tx[k],'Transaction '+k+' differs');eq(actual.input,tx.data,'Calldata differs');
  for(const k of ['value','gas','nonce','maxFeePerGas','maxPriorityFeePerGas','chainId'])if(BigInt(actual[k])!==BigInt(tx[k]))throw Error('Transaction '+k+' differs');
  eq((await c.getBlock({blockNumber:receipt.blockNumber})).hash,receipt.blockHash,'Receipt block was reorganized');
  if(receipt.status!=='success')return {status:'reverted',blockHash:receipt.blockHash};
  await pins(c,plan,a,receipt.blockNumber);await conditions(c,plan,a.postconditions,receipt.blockNumber);
  const confirmations=Number((await c.getBlockNumber({cacheTime:0}))-receipt.blockNumber)+1;
  return {status:confirmations>=3?'confirmed':'pending',blockHash:receipt.blockHash,blockNumber:String(receipt.blockNumber)};
 }));
 if(results.some(r=>r.status==='pending'))return {status:'pending',hash};
 same(results[0],results[1],'RPCs disagree on receipt');return {...results[0],hash};
}

export async function checkPrivateDeployment(plan,cfg){
 const block=await anchor();
 await Promise.all(clients.map(async c=>{
  await pins(c,plan,{},block.number);
  const row=await c.readContract({address:plan.contracts.deployments,abi:abis.deployments,functionName:'get',args:[cfg.deploymentId],blockNumber:block.number});
  for(const [field,value] of Object.entries(cfg.baseline))same(row[field],value,'Private migration baseline changed: '+field);
  if(row.active||BigInt(row.leaseUntil)>block.timestamp)throw Error('Private migration requires an inactive deployment with an expired lease');
 }));
 return {blockNumber:String(block.number),blockHash:block.hash};
}
