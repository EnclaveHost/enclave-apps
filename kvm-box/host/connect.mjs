#!/usr/bin/env node
// Run on the owner's machine. Attest each guest, then pin its TLS key before sending credentials or desktop data.
import fs from 'node:fs';import http from 'node:http';import https from 'node:https';import tls from 'node:tls';import {randomBytes} from 'node:crypto';import {pathToFileURL} from 'node:url';import path from 'node:path';
import {deploymentBinding} from './deployment-binding.mjs';
const args=process.argv.slice(2);const opt=(k)=>{const i=args.indexOf(k);if(i<0||!args[i+1])throw Error('Required '+k);return args[i+1]};
const binding=deploymentBinding(args);
const root=path.resolve(opt('--release')),work=path.resolve(opt('--work')),enclave=path.resolve(opt('--enclave'));
const {judge}=await import(pathToFileURL(path.join(enclave,'isolation/m2/judge.mjs')));const {seedCertChain}=await import(pathToFileURL(path.join(enclave,'relay/snp-verify.mjs')));
const release=JSON.parse(fs.readFileSync(path.join(root,'release.json'))),runtime=JSON.parse(fs.readFileSync(path.join(root,'controller-runtime.json')));
const vcek=fs.readFileSync(opt('--vcek')),minTcb=JSON.parse(fs.readFileSync(opt('--min-tcb')));seedCertChain('Turin',fs.readFileSync(opt('--chain'),'utf8'));
const authorization='Basic '+Buffer.from('owner:'+fs.readFileSync(opt('--password-file'),'utf8').trim()).toString('base64');
class PinnedAgent extends https.Agent{
 constructor(port,pin){super({keepAlive:false});this.port=port;this.pin=pin}
 createConnection(options,cb){let done=false;const finish=(err,s)=>{if(!done){done=true;cb(err,s)}};const s=tls.connect({host:'127.0.0.1',port:this.port,rejectUnauthorized:false,minVersion:'TLSv1.3',session:undefined});s.setTimeout(10000,()=>s.destroy(Error('TLS timeout')));s.once('secureConnect',()=>{const cert=s.getPeerX509Certificate();const key=cert?.publicKey.export({type:'spki',format:'der'});if(!key||!key.equals(this.pin)){s.destroy();finish(Error('Attested TLS key changed'));return}s.setTimeout(0);finish(null,s)});s.once('error',e=>finish(e));}
}
const cache=new Map(),flights=new Map();
async function verified(name){
 const state=JSON.parse(fs.readFileSync(path.join(work,'state.json')))[name];if(state?.status!=='running'||!Number.isInteger(state.hostPort))throw Error(name+' is not ready');
 const cacheKey=name+':'+state.hostPort+':'+state.generation;if(cache.has(cacheKey))return cache.get(cacheKey);
 if(flights.has(cacheKey))return flights.get(cacheKey);
 const task=(async()=>{
 const nonce=randomBytes(32),spec=release.images[name];
 const {doc,spki}=await new Promise((resolve,reject)=>{
  const req=https.get({hostname:'127.0.0.1',port:state.hostPort,path:'/.well-known/enclave-attestation?nonce='+nonce.toString('hex'),agent:false,rejectUnauthorized:false,minVersion:'TLSv1.3',timeout:10000},res=>{
   const cert=res.socket.getPeerX509Certificate();const spki=cert?.publicKey.export({type:'spki',format:'der'});let chunks=[],size=0;
   res.on('data',b=>{size+=b.length;if(size>262144)res.destroy(Error('Oversized attestation'));else chunks.push(b)});res.on('error',reject);res.on('end',()=>{try{if(res.statusCode!==200||!spki)throw Error('Attestation endpoint failed');resolve({doc:JSON.parse(Buffer.concat(chunks)),spki})}catch(e){reject(e)}})
  });req.on('timeout',()=>req.destroy(Error('Attestation timeout')));req.on('error',reject)
 });
 const want={measurement:spec.measurement,appSha:spec.appId,mode:'trusted',kds:false,vcek,minTcb,...(name==='controller'?{runtime}:{}),...(binding.hostData?{hostData:binding.hostData}:{})};
 const verdict=await judge(doc,spki,nonce,want);if(!verdict.gateOpen||verdict.verdict!=='attested')throw Error('Attestation refused: '+JSON.stringify(verdict.reasons));
 const value={port:state.hostPort,agent:new PinnedAgent(state.hostPort,spki)};cache.set(cacheKey,value);console.log(name+': AMD attestation verified; TLS key pinned; binding='+binding.mode);return value;
 })();flights.set(cacheKey,task);try{return await task}finally{flights.delete(cacheKey)}
}
let localOrigin='';
function allowed(req){return req.headers.host===new URL(localOrigin).host&&(!req.headers.origin||req.headers.origin===localOrigin)}
function route(req){const desktop=req.url.startsWith('/desktop/');return {name:desktop?'desktop':'controller',url:desktop?req.url.slice('/desktop'.length):req.url}}
function headers(req){const h={...req.headers,authorization,host:req.headers.host};delete h['proxy-authorization'];delete h['x-forwarded-for'];return h}
const server=http.createServer(async(req,res)=>{
 if(!allowed(req)){res.writeHead(403);res.end('Origin refused');return}
 try{const r=route(req),target=await verified(r.name);const up=https.request({hostname:'127.0.0.1',port:target.port,path:r.url,method:req.method,headers:headers(req),agent:target.agent},reply=>{res.writeHead(reply.statusCode,reply.headers);reply.pipe(res)});up.on('error',e=>{if(!res.headersSent)res.writeHead(502);res.end(e.message)});req.pipe(up);res.on('close',()=>up.destroy())}catch(e){res.writeHead(503,{'content-type':'text/plain','cache-control':'no-store'});res.end(e.message)}
});
server.on('upgrade',async(req,socket,head)=>{
 if(!allowed(req)||!req.url.startsWith('/desktop/')){socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');return}
 try{const target=await verified('desktop');const up=https.request({hostname:'127.0.0.1',port:target.port,path:req.url.slice('/desktop'.length),method:'GET',headers:headers(req),agent:target.agent});up.on('upgrade',(res,remote,rest)=>{socket.write('HTTP/1.1 101 Switching Protocols\r\n'+Object.entries(res.headers).map(([k,v])=>k+': '+v+'\r\n').join('')+'\r\n');if(head.length)remote.write(head);if(rest.length)socket.write(rest);socket.pipe(remote);remote.pipe(socket);socket.on('error',()=>remote.destroy());remote.on('error',()=>socket.destroy());socket.on('close',()=>remote.destroy());remote.on('close',()=>socket.destroy())});up.on('response',res=>{res.resume();socket.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n')});up.on('error',()=>socket.destroy());up.end()}catch(e){socket.end('HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n'+e.message)}
});
const portIndex=args.indexOf('--listen-port');const listenPort=portIndex<0?0:Number(args[portIndex+1]);if(!Number.isInteger(listenPort)||listenPort<0||listenPort>65535)throw Error('Invalid listen port');
server.listen(listenPort,'127.0.0.1',()=>{localOrigin='http://127.0.0.1:'+server.address().port;const tmp=path.join(work,'connector.tmp');fs.writeFileSync(tmp,JSON.stringify({url:localOrigin}));fs.renameSync(tmp,path.join(work,'connector.json'));console.log('KVM Box: '+localOrigin)});

if(!args.includes('--no-moonlight')){const {startMoonlight}=await import('./moonlight.mjs');await startMoonlight({verified,authorization})}
