const $=s=>document.querySelector(s),esc=s=>String(s??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const short=s=>s?s.slice(0,8)+'…'+s.slice(-6):'—';
let state,account,chainId,provider,review,busy=false,selectedSlug;
const providers=[],subscribed=new WeakSet();
function notice(message,error=false){const el=$('#notice');el.textContent=message;el.classList.toggle('error',error);el.hidden=false;}
async function api(route,body){const response=await fetch('/api/'+route,{method:body?'POST':'GET',headers:body?{'Content-Type':'application/json','X-Rollout-Token':state.token}:{},...(body?{body:JSON.stringify(body)}:{})});const data=await response.json();if(!response.ok)throw Error(data.error||'Local request failed');return data;}
async function load(){state=await api('state');render();}
function canPrepare(a){return a.state.status==='ready'&&(a.dependsOn||[]).every(id=>['confirmed','uploaded'].includes(state.queue.actions.find(x=>x.id===id)?.state.status))&&(a.kind!=='snapshot-update'||(state.migration?.status==='verified'&&state.private?.verified));}
function render(){
 const p=state.plan,inv=state.inventory,verified=state.queue.actions.filter(a=>['confirmed','uploaded'].includes(a.state.status)).length;
 $('#banner-title').textContent=p.status==='ready'?(p.queueTitle||'Reviewed actions are ready'):'Release preparation is incomplete';
 $('#banner-detail').textContent=p.status==='ready'?(p.scopeNote||'Review each action before opening its wallet prompt.'):(p.scopeNote||'Release verification is pending. No signing action is enabled.');
 $('#metrics').innerHTML=[['Applications in scope',p.apps.length,'Includes KVM Box Native'],['Deployment references in scope',inv.deployments.filter(d=>p.apps.some(a=>a.deploymentIds.includes(d.id))).length,'Read from the Base ledger'],['Actions ready to sign',state.queue.actions.filter(canPrepare).length,'Gated by the pinned release plan'],['Completed signing actions',verified,'No automatic wallet requests']].map(([label,note,sub])=>`<div class="metric"><label>${esc(label)}</label><strong>${esc(note)}</strong><small>${esc(sub)}</small></div>`).join('');
 $('#phases').innerHTML=p.phases.map((ph,i)=>`<div class="phase"><div class="phase-top"><span class="phase-number">0${i+1}</span><span class="phase-dot"></span></div><strong>${esc(ph.title)}</strong><p>${esc(ph.detail)}</p></div>`).join('');
 const date=new Date(inv.checkedAt),age=Math.max(0,Math.floor((Date.now()-date)/60000));
 $('#snapshot').textContent=`Last successful chain read: ${date.toLocaleString()} · block ${Number(inv.blockNumber).toLocaleString()} · ${age} min old${age>10?' — refresh before relying on this snapshot':''}${state.refreshError?' · REFRESH FAILED':''}`;
 $('#apps').innerHTML=p.apps.map(row=>{
  const apps=inv.apps.filter(a=>row.catalogAppIds.includes(a.appId)),deps=inv.deployments.filter(d=>row.deploymentIds.includes(d.id));
  return `<tr><td><button class="app-button" data-app="${esc(row.slug)}">${esc(row.slug)} ↗</button><span class="app-type">${row.slug==='kvm-box-native'?'Native guest':row.slug==='sia-gateway'?'Keyless tunnel':row.slug==='sia-s3'?'S3 storage endpoint':'Application'}</span></td><td>${apps.map(a=>`<code>${esc(a.latest?.version||'—')}</code>`).join('<br>')||'<span class="mini">Not published</span>'}</td><td>${deps.map(d=>`<code>${esc(d.version?.version||'—')}</code><span class="mini">${esc(short(d.id))}${d.active?' · active':' · inactive'}${d.owner.toLowerCase()!==p.wallet.toLowerCase()?' · '+(row.slug==='kvm-box-native'?'SessionVault':'other owner'):''}</span>`).join('')||'<span class="mini">No selected deployment</span>'}</td><td>${esc(row.intent)}</td><td><span class="badge ${['tested','ready','verification-only'].includes(row.status)?'neutral':'amber'}">${esc(({tested:'Tests passed',unsupported:'Unsupported',ready:'Ready','verification-only':'Verify only'})[row.status]||'Blocked')}</span></td></tr>`;
 }).join('');
 $('#gates').innerHTML=p.gates.map((g,i)=>`<article class="gate"><div class="gate-top"><span class="gate-index">0${i+1}</span><h3>${esc(g.title)}</h3><span class="badge ${g.status==='verified'?'neutral':'amber'}">${g.status==='verified'?'Passed':'Pending'}</span></div><p>${esc(g.detail)}</p></article>`).join('');
 $('#gate-count').textContent=p.gates.filter(g=>g.status!=='verified').length+' unresolved prerequisites';
 $('#expected-wallet').textContent=p.wallet;
 $('#plan-hash').textContent='Plan SHA-256 · '+short(state.queue.planHash);
 if(!state.queue.actions.length){$('#queue').innerHTML='<div class="empty-queue"><span class="badge amber">Waiting for a qualified release</span><h3>There is nothing safe to sign yet.</h3><p>The queue will be populated from frozen release artifacts and verified migration evidence. There is no checkbox or wallet signature that bypasses these prerequisites.</p><a href="#readiness">Review the prerequisites ↑</a></div>';}
 else{$('#queue').innerHTML=state.queue.actions.map(a=>`<article class="queue-action"><span class="badge neutral">${esc(a.kind)} · ${esc(a.state.status)}</span><h3>${esc(a.title)}</h3><p>${esc(a.description||'')}</p>${a.state.hash?`<p>Transaction: <code>${esc(a.state.hash)}</code></p>`:''}${a.state.status==='ready'?`<button class="primary" data-prepare="${esc(a.id)}" ${canPrepare(a)?'':'disabled'}>${canPrepare(a)?'Review request':'Waiting for prerequisites'}</button>`:''}${a.state.status==='review'?`<button class="secondary" data-resume="${esc(a.id)}">Resume review</button>`:''}${['awaiting-wallet','submitted'].includes(a.state.status)&&a.kind!=='upload'?`<p>If a wallet result was lost, recover its hash. Do not send the transaction again.</p><input aria-label="Transaction hash" data-hash="${esc(a.id)}" placeholder="0x…" value="${esc(a.state.hash||'')}" ${a.state.hash?'readonly':''}><button class="secondary" data-verify="${esc(a.id)}">Verify transaction</button>`:''}${a.state.status==='awaiting-wallet'&&a.kind==='upload'?'<p>Upload authorization is unresolved. Keep the wallet prompt open, or report its explicit rejection from the original tab.</p>':''}</article>`).join('');}
 const pr=state.private;
 $('#private-controls').innerHTML=pr?`<p>Status: <strong>${esc(pr.status)}</strong> · ${pr.keyPrepared?'Local snapshot key retained privately':'Owner read required'}</p>${['review','awaiting-wallet','submitting'].includes(pr.status)?`<p>A private signing request is open. ${pr.status==='review'?'<button class="secondary" data-private-resume>Resume private review</button>':'Resolve its wallet prompt before continuing.'}</p>`:`<button class="secondary" data-private="read">Review owner read</button> <button class="secondary" data-private="write" ${pr.ready?'':'disabled'}>Review key provisioning</button> <button class="secondary" data-private="verify">Verify stored key</button>`}`:'<p>Private provisioning is being prepared.</p>';
 if(pr){const m=state.migration;$('#private-controls').insertAdjacentHTML('beforeend',`<p>Legacy snapshot migration: <strong>${esc(m?.status||'idle')}</strong></p><button class="primary" data-migrate ${pr.verified&&m?.status==='idle'?'':'disabled'}>Encrypt and verify legacy snapshots</button>${m?.status==='verified'?'<p>Encrypted copies verified. The deployment update can be reviewed after publication.</p>':''}${m?.status==='held'?`<p>${esc(m.error)}</p>`:''}`);}
 if(selectedSlug)detail(selectedSlug);walletView();
}
function detail(slug){
 selectedSlug=slug;const row=state.plan.apps.find(r=>r.slug===slug),apps=state.inventory.apps.filter(a=>row.catalogAppIds.includes(a.appId)),deps=state.inventory.deployments.filter(d=>row.deploymentIds.includes(d.id));
 $('#app-detail').hidden=false;$('#app-detail').innerHTML=`<button class="close" id="close-detail" aria-label="Close app details">×</button><h3>${esc(slug)}</h3><p>${esc(row.detail)}</p><p><strong>Candidate release:</strong> ${esc(row.candidateVersion||'Still under qualification')}${row.artifact?` · <code>${esc(row.artifact.cid)}</code>`:''}</p><dl>${apps.map(a=>`<dt>Catalog app</dt><dd>${esc(a.appId)}</dd><dt>Publisher</dt><dd>${esc(a.publisher)}</dd><dt>Current CID</dt><dd>${esc(a.latest?.cid)}</dd>`).join('')}${deps.map(d=>`<dt>Deployment</dt><dd>${esc(d.id)}</dd><dt>Owner</dt><dd>${esc(d.owner)}</dd><dt>Current reference</dt><dd>${esc(d.appRef)}</dd>`).join('')}<dt>Required evidence</dt><dd>${esc(row.gates.join(', ')||'Post-cutover integration verification')}</dd></dl>`;
}
function walletView(){
 const good=account&&account.toLowerCase()===state?.plan.wallet.toLowerCase(),base=Number(chainId)===8453;
 $('#wallet-status').textContent=account?short(account):'Not connected';
 $('#wallet-help').textContent=account?(!good?'This account does not match the reviewed signer. Select the expected wallet account.':!base?'Wallet connected on another chain. Switch to Base before signing.':'Expected wallet connected on Base. Each signature still requires your review.'):'Connecting only reads your account and network. Signatures require a separate review.';
 $('#connect').textContent=account?'Reconnect wallet':'Connect wallet';$('#switch').hidden=!account||base;
}
function discover(detail){if(!detail?.provider||providers.some(p=>p.provider===detail.provider))return;providers.push(detail);$('#wallet-select').hidden=providers.length<2;$('#wallet-select').innerHTML=providers.map((p,i)=>`<option value="${i}">${esc(p.info?.name||'Browser wallet')}</option>`).join('');}
window.addEventListener('eip6963:announceProvider',event=>discover(event.detail));window.dispatchEvent(new Event('eip6963:requestProvider'));if(window.ethereum)discover({info:{name:'Browser wallet'},provider:window.ethereum});
async function connect(){
 provider=providers[Number($('#wallet-select').value)||0]?.provider||window.ethereum;
 if(!provider)throw Error('No wallet extension is available in this browser. Open this same localhost URL in your regular browser with MetaMask or your signing wallet.');
 [account]=await provider.request({method:'eth_requestAccounts'});chainId=await provider.request({method:'eth_chainId'});
 if(!subscribed.has(provider)){provider.on?.('accountsChanged',a=>{account=a[0];walletView();});provider.on?.('chainChanged',c=>{chainId=c;walletView();});subscribed.add(provider);}walletView();
}
async function walletContext(){if(!provider||!account)throw Error('Connect your wallet first.');const accounts=await provider.request({method:'eth_accounts'});const c=await provider.request({method:'eth_chainId'});if(accounts[0]?.toLowerCase()!==state.plan.wallet.toLowerCase()||Number(c)!==8453)throw Error('Select the expected wallet account on Base.');return {account:accounts[0],chainId:c};}
function showReview(a,s){review={id:a.id,action:a,...s};$('#review-title').textContent=a.title;$('#request-preview').textContent=JSON.stringify({action:a.description||a.title,...s.request},null,2);$('#review-error').textContent='';$('#review-dialog').showModal();}
async function prepare(id){const context=await walletContext(),s=await api('prepare',{id,...context});await load();showReview(state.queue.actions.find(a=>a.id===id),s);}
async function cancelReview(){if(!review)return;await api(review.action.private?'private/cancel':'cancel',{id:review.id,attemptId:review.attemptId});review=null;$('#review-dialog').close();await load();}
async function send(){
 const ctx=await walletContext(),r=review;const dispatched=await api(r.action.private?'private/dispatch':'dispatch',{id:r.id,attemptId:r.attemptId,...ctx});
 $('#review-dialog').close();review=null;await load();
 let answer;
 try{answer=(r.action.kind==='upload'||r.action.private)?await provider.request({method:'personal_sign',params:[dispatched.request.hexMessage,ctx.account]}):await provider.request({method:'eth_sendTransaction',params:[dispatched.request.transaction]});}
 catch(e){if(e.code===4001){await api(r.action.private?'private/reject':'reject',{id:r.id,attemptId:r.attemptId,code:4001});notice('Wallet request rejected. Nothing will be retried automatically.');}else notice('The wallet result is uncertain. The request is held. Recover its transaction hash before continuing.',true);await load();return;}
 try{await api(r.action.private?'private/submit':'result',{...ctx,id:r.id,attemptId:r.attemptId,...((r.action.kind==='upload'||r.action.private)?{signature:answer}:{hash:answer})});notice(r.action.private?'Private operation recorded. Verify the stored key before migration.':r.action.kind==='upload'?'Upload CID and CAR verified.':'Transaction recorded. Verify its confirmations before the next step.');}
 catch(e){notice(e.message+(!r.action.private&&r.action.kind!=='upload'?' Wallet returned transaction hash: '+answer:''),true);}
 await load();
}
async function run(fn){if(busy)return;busy=true;try{await fn();}catch(e){notice(e.message,true);if($('#review-dialog').open)$('#review-error').textContent=e.message;}finally{busy=false;}}
$('#connect').addEventListener('click',()=>run(connect));$('#switch').addEventListener('click',()=>run(async()=>{await provider.request({method:'wallet_switchEthereumChain',params:[{chainId:'0x2105'}]});chainId=await provider.request({method:'eth_chainId'});walletView();}));
$('#refresh').addEventListener('click',()=>run(async()=>{const button=$('#refresh');button.disabled=true;button.textContent='Reading chain…';try{state=await api('refresh',{});render();notice('Chain inventory refreshed. Readiness and signing gates were not changed.');}finally{button.disabled=false;button.textContent='↻ Refresh chain';}}));
$('#export').addEventListener('click',()=>run(async()=>{const report=await api('report'),url=URL.createObjectURL(new Blob([JSON.stringify(report,null,2)+'\n'],{type:'application/json'}));const a=document.createElement('a');a.href=url;a.download='sia-rollout-review.json';a.click();setTimeout(()=>URL.revokeObjectURL(url),1000);notice('Review exported, including the observed baseline and readiness evidence.');}));
$('#apps').addEventListener('click',e=>{const button=e.target.closest('[data-app]');if(button)detail(button.dataset.app);});
$('#app-detail').addEventListener('click',e=>{if(e.target.closest('#close-detail')){selectedSlug=null;$('#app-detail').hidden=true;}});
$('#queue').addEventListener('click',e=>run(async()=>{const p=e.target.closest('[data-prepare]'),r=e.target.closest('[data-resume]'),v=e.target.closest('[data-verify]');if(p)await prepare(p.dataset.prepare);if(r){const a=state.queue.actions.find(a=>a.id===r.dataset.resume);showReview(a,a.state);}if(v){const a=state.queue.actions.find(a=>a.id===v.dataset.verify),input=[...document.querySelectorAll('[data-hash]')].find(n=>n.dataset.hash===a.id);await api('verify',{id:a.id,attemptId:a.state.attemptId,hash:input.value.trim()});await load();}}));
$('#private-controls').addEventListener('click',e=>run(async()=>{
 const migrate=e.target.closest('[data-migrate]');if(migrate){await api('migrate',await walletContext());await load();notice('Encrypted snapshot copies were downloaded, authenticated and compared with the originals.');return;}
 const button=e.target.closest('[data-private]'),resume=e.target.closest('[data-private-resume]');
 if(button){const ctx=await walletContext(),s=await api('private/prepare',{...ctx,operation:button.dataset.private});await load();showReview({id:'private',private:true,title:'Review private snapshot authorization',description:s.request.purpose},s);}
 if(resume){const s=state.private;showReview({id:'private',private:true,title:'Review private snapshot authorization',description:s.request.purpose},s);}
}));
$('#send').addEventListener('click',()=>run(send));$('#close-review').addEventListener('click',()=>run(cancelReview));$('#cancel-review').addEventListener('click',()=>run(cancelReview));$('#review-dialog').addEventListener('cancel',e=>{e.preventDefault();run(cancelReview);});
const observer=new IntersectionObserver(entries=>{for(const entry of entries)if(entry.isIntersecting){document.querySelectorAll('nav a').forEach(a=>a.classList.toggle('active',a.hash==='#'+entry.target.id));}},{rootMargin:'-10% 0px -65% 0px'});document.querySelectorAll('main section').forEach(el=>observer.observe(el));
load().catch(e=>notice('The local rollout server could not be read: '+e.message,true));

// Keep an idle review page current when release evidence changes locally.
// Never replace a wallet review, recovery input, or in-flight signing state.
setInterval(async()=>{
 if(busy||review||document.hidden||state?.queue.open||['review','awaiting-wallet','submitting'].includes(state?.private?.status))return;
 busy=true;
 try{const next=await api('state');if(!state||next.queue.planHash!==state.queue.planHash||next.inventory.blockNumber!==state.inventory.blockNumber){state=next;render();}}
 catch{/* The explicit refresh/export controls report connection failures. */}
 finally{busy=false;}
},15000);
