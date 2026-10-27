#!/usr/bin/env node
// Owner-local transport for an already admitted native desktop. WebPKI validation
// AND the operator's existing instance SPKI pin are required before credentials
// leave this process. This does not replace the platform's attestation/admission.
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import tls from 'node:tls';
import net from 'node:net';
import crypto from 'node:crypto';
import {spawn} from 'node:child_process';
import {pathToFileURL} from 'node:url';

export function allowedRequest(req, origin, token) {
  if(req.headers.host !== new URL(origin).host) return false;
  if(req.headers.origin && req.headers.origin !== origin) return false;
  if(req.headers['sec-fetch-site'] === 'cross-site') return false;
  return (req.headers.cookie || '').split(';').some(x => x.trim() === 'native_desktop='+token);
}
export function desktopPath(url) {
  if(typeof url !== 'string' || !url.startsWith('/desktop/') || /[\\\r\n]/.test(url)) return false;
  try { const p=new URL(url,'http://localhost'); return p.pathname.startsWith('/desktop/') && !/%(?:2e|2f|5c)/i.test(p.pathname); } catch { return false; }
}
export function pinnedIdentity(host, cert, pin) {
  const err=tls.checkServerIdentity(host,cert); if(err) return err;
  const key=new crypto.X509Certificate(cert.raw).publicKey.export({type:'spki',format:'der'});
  if(crypto.createHash('sha256').update(key).digest('hex') !== pin) return Error('Native desktop instance key changed; reverify the instance');
}
export async function start(config) {
  const {hostname,spkiSha256,sshTarget,guestPort,credentialsFile,stateFile}=config;
  if(!/^[a-z0-9.-]+$/.test(hostname)||!/^([a-f0-9]{64})$/.test(spkiSha256)||!Number.isInteger(guestPort)||guestPort<1||guestPort>65535||!/^[-a-zA-Z0-9_.@]+$/.test(sshTarget)||sshTarget.startsWith('-'))throw Error('Invalid connection configuration');
  const env=Object.fromEntries(fs.readFileSync(credentialsFile,'utf8').split('\n').filter(l=>l && !l.startsWith('#') && l.includes('=')).map(l=>{const i=l.indexOf('=');return [l.slice(0,i),l.slice(i+1).trim().replace(/^(['"])(.*)\1$/,'$2')]}));
  if(!env.KVMBOX_OWNER_PASSWORD)throw Error('Owner credential missing');
  const authorization='Basic '+Buffer.from('owner:'+env.KVMBOX_OWNER_PASSWORD).toString('base64');
  const probe=net.createServer();await new Promise(r=>probe.listen(0,'127.0.0.1',r));const tunnelPort=probe.address().port;await new Promise(r=>probe.close(r));
  const ssh=spawn('ssh',['-N','-o','BatchMode=yes','-o','StrictHostKeyChecking=yes','-o','ExitOnForwardFailure=yes','-o','ConnectTimeout=6','-o','ServerAliveInterval=15','-o','ServerAliveCountMax=3','-L',`127.0.0.1:${tunnelPort}:127.0.0.1:${guestPort}`,sshTarget],{stdio:['ignore','ignore','pipe']});
  let sshEnded=false;ssh.on('exit',()=>{sshEnded=true});ssh.stderr.resume();
  for(let i=0;i<60;i++){
    if(sshEnded)throw Error('SSH transport failed');
    const ready=await new Promise(r=>{const s=net.connect(tunnelPort,'127.0.0.1');s.once('connect',()=>{s.destroy();r(true)});s.once('error',()=>r(false));});
    if(ready)break;if(i===59){ssh.kill();throw Error('SSH transport timed out')};await new Promise(r=>setTimeout(r,100));
  }
  const agent=new https.Agent({keepAlive:true,maxSockets:16,maxCachedSessions:0,minVersion:'TLSv1.3',servername:hostname,rejectUnauthorized:true,checkServerIdentity:(_host,cert)=>pinnedIdentity(hostname,cert,spkiSha256)});
  const upstream=(req)=>({hostname:'127.0.0.1',port:tunnelPort,servername:hostname,minVersion:'TLSv1.3',rejectUnauthorized:true,checkServerIdentity:(_host,cert)=>pinnedIdentity(hostname,cert,spkiSha256),agent,path:req.url,method:req.method,headers:{...req.headers,host:hostname,authorization,cookie:undefined,origin:undefined,'proxy-authorization':undefined}});
  // Check trust before exposing the local viewer. No owner credentials sent here.
  try {await new Promise((resolve,reject)=>{const r=https.get({...upstream({url:'/health',method:'GET',headers:{}}),headers:{host:hostname},timeout:10000},s=>{s.resume();s.on('end',resolve)});r.on('error',reject);r.on('timeout',()=>r.destroy(Error('TLS readiness timeout')))});}catch(e){ssh.kill();agent.destroy();throw e;}
  const token=crypto.randomBytes(32).toString('hex'),launch=crypto.randomBytes(32).toString('hex');let origin='';
  const sockets=new Set();
  const server=http.createServer((req,res)=>{
    res.setHeader('Cache-Control','no-store');res.setHeader('Referrer-Policy','no-referrer');
    if(req.method==='GET' && req.url==='/connect/'+launch && req.headers.host===new URL(origin).host && (!req.headers.origin||req.headers.origin===origin) && req.headers['sec-fetch-site']!=='cross-site'){
      res.writeHead(303,{'Set-Cookie':`native_desktop=${token}; HttpOnly; SameSite=Strict; Path=/`,'Location':'/desktop/vnc.html?autoconnect=true&resize=remote&path=desktop/websockify&quality=6&compression=2'});res.end();return;
    }
    if(req.method!=='GET'||!desktopPath(req.url)||!allowedRequest(req,origin,token)){res.writeHead(403);res.end('Local desktop access refused');return;}
    const options=upstream(req);for(const k of Object.keys(options.headers))if(options.headers[k]===undefined)delete options.headers[k];
    const up=https.request(options,reply=>{const headers={...reply.headers,'cache-control':'no-store','referrer-policy':'no-referrer'};delete headers['set-cookie'];res.writeHead(reply.statusCode,headers);reply.pipe(res)});
    up.on('error',()=>{if(!res.headersSent)res.writeHead(502);res.end('Encrypted desktop transport unavailable')});up.setTimeout(30000,()=>up.destroy());req.pipe(up);res.on('close',()=>up.destroy());
  });
  server.on('connection',s=>{sockets.add(s);s.on('close',()=>sockets.delete(s))});
  server.on('upgrade',(req,socket,head)=>{
    if(req.method!=='GET'||req.url!=='/desktop/websockify'||!allowedRequest(req,origin,token)){socket.end('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');return;}
    const options=upstream(req);for(const k of Object.keys(options.headers))if(options.headers[k]===undefined)delete options.headers[k];
    const up=https.request(options);up.setTimeout(10000,()=>up.destroy());
    up.on('upgrade',(reply,remote,rest)=>{
      up.setTimeout(0);socket.setNoDelay(true);remote.setNoDelay(true);
      socket.write('HTTP/1.1 101 Switching Protocols\r\n'+Object.entries(reply.headers).map(([k,v])=>k+': '+v+'\r\n').join('')+'\r\n');
      if(head.length)remote.write(head);if(rest.length)socket.write(rest);socket.pipe(remote).pipe(socket);
      socket.on('error',()=>remote.destroy());remote.on('error',()=>socket.destroy());socket.on('close',()=>remote.destroy());remote.on('close',()=>socket.destroy());
    });
    up.on('response',reply=>{reply.resume();socket.end('HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n')});up.on('error',()=>socket.destroy());socket.on('close',()=>up.destroy());up.end();
  });
  await new Promise(r=>server.listen(config.listenPort||0,'127.0.0.1',r));origin='http://127.0.0.1:'+server.address().port;
  if(config.moonlightAddress){
    if(!/^127\.0\.0\.[2-9]$/.test(config.moonlightAddress))throw Error('Moonlight must bind to an owner-local loopback address');
    const {startMoonlight}=await import('./moonlight.mjs');
    await startMoonlight({verified:async()=>({port:tunnelPort,agent,hostname,pathPrefix:'/desktop'}),authorization,address:config.moonlightAddress});
  }
  fs.writeFileSync(stateFile,JSON.stringify({url:origin+'/connect/'+launch,origin,hostname,spkiSha256,pid:process.pid,tunnelPort,moonlightAddress:config.moonlightAddress},null,2)+'\n',{mode:0o600});fs.chmodSync(stateFile,0o600);
  console.log('Native desktop connector ready at '+origin+'; WebPKI and instance key verified');
  const close=()=>{server.close();for(const s of sockets)s.destroy();agent.destroy();ssh.kill();};
  ssh.once('exit',()=>{close();process.exit(1)});process.once('SIGTERM',()=>{close();process.exit(0)});process.once('SIGINT',()=>{close();process.exit(0)});
  return {origin,close};
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href){start(JSON.parse(fs.readFileSync(process.argv[2]))).catch(e=>{console.error(e.message);process.exitCode=1})}
