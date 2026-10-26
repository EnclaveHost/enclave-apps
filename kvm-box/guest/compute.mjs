// Runs inside the desktop CVM. No request data leaves until remote SNP attestation passes.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import tls from 'node:tls';
import net from 'node:net';
import {randomBytes} from 'node:crypto';
import {judge} from './enclave/isolation/m2/judge.mjs';
import {seedCertChain} from './enclave/relay/snp-verify.mjs';
const config=JSON.parse(fs.readFileSync('/compute/services.json'));
seedCertChain('Turin',fs.readFileSync('/compute/amd-chain.pem','utf8'));
const vcek=fs.readFileSync('/compute/vcek.der'),minTcb=JSON.parse(fs.readFileSync('/compute/min-tcb.json'));
const cache=new Map(),flights=new Map();
function nativeCompute(request) {
 return new Promise((resolve,reject)=>{
  const data=Buffer.from(JSON.stringify(request));if(data.length>8*1024*1024){reject(Error('Request too large'));return}
  const header=Buffer.alloc(4);header.writeUInt32BE(data.length);
  const socket=net.createConnection('/run/shield/compute.sock');let bytes=Buffer.alloc(0),size=null,done=false;
  const fail=e=>{if(!done){done=true;socket.destroy();reject(e)}};
  socket.setTimeout(21*60*1000,()=>fail(Error('Compute timeout')));
  socket.on('connect',()=>socket.write(Buffer.concat([header,data])));
  socket.on('error',fail);socket.on('end',()=>{if(!done)fail(Error('Incomplete compute response'))});
  socket.on('data',chunk=>{
   bytes=Buffer.concat([bytes,chunk]);
   if(size===null&&bytes.length>=4){size=bytes.readUInt32BE(0);if(!size||size>16*1024*1024){fail(Error('Invalid compute response'));return}}
   if(size!==null&&bytes.length>=size+4){try{const value=JSON.parse(bytes.subarray(4,size+4));done=true;socket.destroy();resolve(value)}catch(e){fail(e)}}
  });
 });
}
class PinnedAgent extends https.Agent {
 constructor(endpoint,pin){super({keepAlive:false});this.endpoint=endpoint;this.pin=pin}
 createConnection(_opts,cb){let done=false;const finish=(e,s)=>{if(!done){done=true;cb(e,s)}};const s=tls.connect({host:this.endpoint.hostname,port:this.endpoint.port||443,rejectUnauthorized:false,minVersion:'TLSv1.3'});s.setTimeout(10000,()=>s.destroy(Error('TLS timeout')));s.once('secureConnect',()=>{const key=s.getPeerX509Certificate()?.publicKey.export({type:'spki',format:'der'});if(!key?.equals(this.pin)){s.destroy();finish(Error('Service TLS key differs from attestation'));return}s.setTimeout(0);finish(null,s)});s.once('error',e=>finish(e))}
}
async function verify(name){
 const spec=config.services[name];if(!spec)throw Error('Unknown service');
 const existing=cache.get(name);if(existing&&Date.now()-existing.time<60000)return existing;
 if(flights.has(name))return flights.get(name);
 const promise=(async()=>{
  const endpoint=new URL(spec.endpoint);if(endpoint.protocol!=='https:')throw Error('HTTPS required');const nonce=randomBytes(32);
  const {doc,spki}=await new Promise((resolve,reject)=>{
   const req=https.get(new URL('/.well-known/enclave-attestation?nonce='+nonce.toString('hex'),endpoint),{agent:false,rejectUnauthorized:false,minVersion:'TLSv1.3',timeout:10000},res=>{
    const spki=res.socket.getPeerX509Certificate()?.publicKey.export({type:'spki',format:'der'});let size=0,chunks=[];
    res.on('data',b=>{size+=b.length;if(size>262144)res.destroy(Error('Oversized attestation'));else chunks.push(b)});res.on('error',reject);res.on('end',()=>{try{if(res.statusCode!==200||!spki)throw Error('Missing attestation');resolve({doc:JSON.parse(Buffer.concat(chunks)),spki})}catch(e){reject(e)}})
   });req.on('error',reject);req.on('timeout',()=>req.destroy(Error('Attestation timeout')))
  });
  const verdict=await judge(doc,spki,nonce,{measurement:spec.measurement,appSha:spec.appId,hostData:spec.hostData,runtime:spec.runtime,mode:'trusted',vcek,minTcb,kds:false});
  if(!verdict.gateOpen||verdict.verdict!=='attested')throw Error('Service attestation refused: '+verdict.reasons.join('; '));
  const value={endpoint,agent:new PinnedAgent(endpoint,spki),time:Date.now()};cache.set(name,value);console.log(name+': service identity verified; TLS key pinned');return value;
 })();flights.set(name,promise);try{return await promise}finally{flights.delete(name)}
}
const allowed={images:new Set(['GET /info','GET /ping','POST /generate','POST /v1/images/generations','POST /v1/images/upscale']),chat:new Set(['GET /models','GET /v1/models','GET /ping','POST /v1/chat/completions'])};
http.createServer(async(req,res)=>{
 if(req.headers.host!=='127.0.0.1:8765'||(req.headers.origin&&req.headers.origin!=='http://127.0.0.1:8765')){res.writeHead(403);res.end('Origin refused');return}
 res.setHeader('Cache-Control','no-store');
 if(req.method==='GET'&&req.url==='/native/') {res.setHeader('Content-Type','text/html');res.end(fs.readFileSync('/compute/native.html'));return}
 if((req.method==='GET'&&req.url==='/api/native')||(req.method==='POST'&&req.url==='/native/compute')){
  try{
   let request={op:'status'};
   if(req.method==='POST'){
    let size=0,chunks=[];for await(const b of req){size+=b.length;if(size>65536)throw Error('Request too large');chunks.push(b)}
    request=JSON.parse(Buffer.concat(chunks));if(!['llama','sd'].includes(request.op))throw Error('Unsupported engine');
   }
   const reply=await nativeCompute(request);res.setHeader('Content-Type','application/json');res.end(JSON.stringify(reply));
  }catch(e){res.writeHead(503);res.end('Native compute unavailable: '+e.message)}return;
 }
 if(req.method==='GET'&&req.url==='/'){res.setHeader('Content-Type','text/html');res.end(fs.readFileSync('/compute/index.html'));return}
 if(req.method==='GET'&&req.url==='/api/services'){
  const services=await Promise.all(Object.entries(config.services).map(async([name,s])=>{try{await verify(name);return {name,label:s.label,verified:true,baseUrl:`http://127.0.0.1:8765/services/${name}`,capabilities:s.capabilities}}catch(e){return {name,label:s.label,verified:false,error:e.message}}}));res.setHeader('Content-Type','application/json');res.end(JSON.stringify({services}));return;
 }
 const match=/^\/services\/([a-z0-9-]+)(\/[^?]*)$/.exec(req.url);
 if(!match||!config.services[match[1]]||!allowed[match[1]]?.has(req.method+' '+match[2])){res.writeHead(404);res.end('Unknown compute route');return}
 try{
  const target=await verify(match[1]);
  const up=https.request({hostname:target.endpoint.hostname,port:target.endpoint.port||443,path:match[2],method:req.method,headers:{host:target.endpoint.host,'content-type':req.headers['content-type']||'application/json',...(req.headers.authorization?{authorization:req.headers.authorization}:{})},agent:target.agent},reply=>{res.writeHead(reply.statusCode,{'content-type':reply.headers['content-type']||'application/json'});reply.pipe(res)});
  up.setTimeout(600000,()=>up.destroy(Error('Compute request timed out')));
  up.on('error',e=>{cache.delete(match[1]);if(!res.headersSent)res.writeHead(502);res.end(e.message)});
  let size=0;req.on('data',b=>{size+=b.length;if(size>32*1024*1024){up.destroy(Error('Request too large'));req.destroy()}else if(!up.write(b))req.pause()});up.on('drain',()=>req.resume());req.on('end',()=>up.end());req.on('aborted',()=>up.destroy());res.on('close',()=>up.destroy());
 }catch(e){res.writeHead(503);res.end(e.message)}
}).listen(8765,'127.0.0.1',()=>console.log('Compute services: http://127.0.0.1:8765'));
