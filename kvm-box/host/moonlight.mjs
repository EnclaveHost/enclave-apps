// Owner-side adapters. Every byte crossing the VM host uses the attested TLS tunnel.
import net from 'node:net';
import dgram from 'node:dgram';
import https from 'node:https';

export async function startMoonlight({verified,authorization,address='127.0.0.2'}) {
 const listeners=[];
 async function tunnel(transport,port) {
  const target=await verified('desktop');
  return new Promise((resolve,reject)=>{
   const req=https.request({hostname:'127.0.0.1',port:target.port,path:`${target.pathPrefix||''}/api/moonlight/tunnel?transport=${transport}&port=${port}`,headers:{host:target.hostname||'kvm-box',authorization,connection:'Upgrade',upgrade:'kvm-box-tunnel'},agent:target.agent});
   const timer=setTimeout(()=>req.destroy(Error('Tunnel timeout')),10000);
   req.once('upgrade',(res,sock,head)=>{clearTimeout(timer);if(head.length)sock.unshift(head);resolve(sock)});
   req.once('response',res=>{clearTimeout(timer);res.resume();reject(Error('Tunnel refused: '+res.statusCode))});
   req.once('error',e=>{clearTimeout(timer);reject(e)});req.end();
  });
 }
 try {
  for(const port of [47984,47989,48010]){
   const server=net.createServer(async local=>{local.on('error',()=>{});local.pause();try{const remote=await tunnel('tcp',port);if(local.destroyed){remote.destroy();return}local.pipe(remote);remote.pipe(local);local.on('error',()=>remote.destroy());remote.on('error',()=>local.destroy());local.on('close',()=>remote.destroy());remote.on('close',()=>local.destroy());local.resume()}catch{local.destroy()}});
   await new Promise((resolve,reject)=>{server.once('error',reject);server.listen(port,address,resolve)});listeners.push(server);
  }
  for(const port of [47998,47999,48000]){
   const socket=dgram.createSocket('udp4'),peers=new Map();
   socket.on('message',(data,peer)=>{
    const key=peer.address+':'+peer.port;let state=peers.get(key);
    if(!state){
     if(peers.size>=8)return;
     state={queue:[],bytes:0,last:Date.now(),buf:Buffer.alloc(0),remote:null};peers.set(key,state);
     const close=()=>{if(peers.get(key)===state)peers.delete(key);state.remote?.destroy()};
     tunnel('udp',port).then(remote=>{
      if(peers.get(key)!==state){remote.destroy();return}state.remote=remote;
      remote.on('error',close);remote.on('close',close);
      remote.on('data',chunk=>{state.buf=Buffer.concat([state.buf,chunk]);while(state.buf.length>=2){const n=state.buf.readUInt16BE(0);if(!n){close();return}if(state.buf.length<n+2)break;socket.send(state.buf.subarray(2,n+2),peer.port,peer.address);state.buf=state.buf.subarray(n+2)}});
      for(const packet of state.queue)remote.write(packet);state.queue=[];state.bytes=0;
     }).catch(close);
    }
    state.last=Date.now();if(!data.length||data.length>65535)return;const frame=Buffer.allocUnsafe(data.length+2);frame.writeUInt16BE(data.length);data.copy(frame,2);
    if(state.remote){if(state.remote.writableLength<1024*1024)state.remote.write(frame)}else if(state.bytes+frame.length<65536){state.queue.push(frame);state.bytes+=frame.length}
   });
   const timer=setInterval(()=>{for(const [key,s]of peers)if(Date.now()-s.last>90000){s.remote?.destroy();peers.delete(key)}},15000);timer.unref();
   socket.on('error',e=>console.error('Moonlight UDP: '+e.message));
   await new Promise((resolve,reject)=>{socket.once('error',reject);socket.bind(port,address,resolve)});listeners.push(socket);
  }
 }catch(e){for(const s of listeners)s.close();throw e}
 console.log('Moonlight: add PC '+address+'; pair through KVM Box. Streaming uses attested TLS.');
 return {address};
}
