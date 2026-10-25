import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
import {test} from 'node:test';
const code=readFileSync(new URL('../guest/native.html',import.meta.url),'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];
const flush=()=>new Promise(resolve=>setImmediate(resolve));
function page(path='/desktop/compute/native/'){
 const ids=['status','run','progress','text','image','stats','prompt','engine','mode'];
 const elements=Object.fromEntries(ids.map(id=>[id,{disabled:id==='run',value:'',textContent:'',hidden:false}]));
 elements.prompt.value='keep my prompt';elements.engine.value='llama';elements.mode.value='masked';
 const requests=[],timers=new Map();let timer=0;
 const context=vm.createContext({document:{getElementById:id=>elements[id]},location:{pathname:path},AbortController,
  setTimeout:(fn,ms)=>{timers.set(++timer,{fn,ms});return timer},clearTimeout:id=>timers.delete(id),
  fetch:(url,options)=>new Promise((resolve,reject)=>requests.push({url,options,resolve,reject}))});
 vm.runInContext(code,context);
 return {elements,requests,timers,context,
  reply:async(index,value,status=200)=>{requests[index].resolve({ok:status===200,status,json:async()=>value,text:async()=>JSON.stringify(value)});await flush()},
  poll:()=>{const [id,t]=[...timers].find(([,t])=>t.ms===2000);timers.delete(id);t.fn()}};
}
test('initial busy recovers to ready automatically without losing prompt/results',async()=>{
 const p=page();await p.reply(0,{ok:true,state:'busy'});assert.equal(p.elements.run.disabled,true);
 p.elements.text.textContent='previous result';p.poll();await p.reply(1,{ok:true,state:'ready'});
 assert.equal(p.elements.run.disabled,false);assert.equal(p.elements.prompt.value,'keep my prompt');assert.equal(p.elements.text.textContent,'previous result');
});
test('HTTP and network failures retry and recover',async()=>{
 const p=page();await p.reply(0,{},503);assert.equal(p.elements.run.disabled,true);p.poll();
 p.requests[1].reject(Error('offline'));await flush();assert.match(p.elements.status.textContent,/retrying/);
 p.poll();await p.reply(2,{ok:true,state:'ready'});assert.equal(p.elements.run.disabled,false);
});
test('a job remains disabled until a fresh ready response and ignores stale polls',async()=>{
 const p=page();await p.reply(0,{ok:true,state:'ready'});p.poll();
 const job=p.elements.run.onclick();assert.equal(p.elements.run.disabled,true);
 assert.equal(p.requests[1].options.signal.aborted,true);
 await p.reply(1,{ok:true,state:'ready'});assert.equal(p.elements.run.disabled,true);
 await p.reply(2,{ok:true,text:'Paris',stats:{offloaded:726}});assert.equal(p.elements.run.disabled,true);
 await p.reply(3,{ok:true,state:'ready'});await job;assert.equal(p.elements.run.disabled,false);assert.equal(p.elements.text.textContent,'Paris');
});
test('security retirement after a failed job stays disabled',async()=>{
 const p=page();await p.reply(0,{ok:true,state:'ready'});const job=p.elements.run.onclick();
 await p.reply(1,{ok:false,error:'verification failed'});await p.reply(2,{ok:true,state:'retired'});await job;
 assert.equal(p.elements.run.disabled,true);assert.match(p.elements.status.textContent,/restart/);assert.match(p.elements.progress.textContent,/verification failed/);
});
test('hung status requests abort and retry',async()=>{
 const p=page();const [id,t]=[...p.timers].find(([,t])=>t.ms===5000);p.timers.delete(id);t.fn();
 assert.equal(p.requests[0].options.signal.aborted,true);p.requests[0].reject(Error('aborted'));await flush();
 p.poll();await p.reply(1,{ok:true,state:'ready'});assert.equal(p.elements.run.disabled,false);
});
test('guest-local and connector URLs resolve to their respective status endpoint',()=>{
 for(const [path,endpoint] of [['/native/','/api/native'],['/native','/api/native'],['/desktop/compute/native/','/desktop/compute/api/native']]){
  const p=page(path);assert.equal(p.requests[0].url,endpoint);assert.equal(p.requests[0].options.cache,'no-store');
 }
});
