import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('async function readWarmupResponse(');
const end = html.indexOf('\nfunction pill(', begin);
const ctx = vm.createContext({ TextDecoder });
vm.runInContext(html.slice(begin, end), ctx);
const read = ctx.readWarmupResponse;
const enc = new TextEncoder();
function response(chunks) {
  return new Response(new ReadableStream({ start(c) {
    for (const chunk of chunks) c.enqueue(chunk);
    c.close();
  } }), { headers: { 'content-type': 'application/x-ndjson' } });
}

test('progress arrives before completion, without waiting for the result', async () => {
  let stream, seen;
  const first = new Promise(resolve => { seen = resolve; });
  const res = new Response(new ReadableStream({ start(c) { stream = c; } }), {
    headers: { 'content-type': 'application/x-ndjson' },
  });
  const pending = read(res, seen);
  stream.enqueue(enc.encode('{"status":"prefilling 4 of 20 prompt tokens"}\n'));
  assert.equal(await first, 'prefilling 4 of 20 prompt tokens');
  stream.enqueue(enc.encode('{"result":{"ok":true,"prefix":{"parked":true}}}\n'));
  stream.close();
  assert.equal((await pending).prefix.parked, true);
});

test('all byte boundaries, including UTF-8, preserve progress and final JSON', async () => {
  const bytes = enc.encode('{"status":"Model loaded…"}\n{"result":{"ok":true}}\n');
  for (let i = 0; i <= bytes.length; i++) {
    const statuses = [];
    const result = await read(response([bytes.slice(0, i), bytes.slice(i)]), s => statuses.push(s));
    assert.equal(result.ok, true);
    assert.deepEqual(statuses, ['Model loaded…']);
  }
});

test('a truncated warmup is not reported as a ready model', async () => {
  await assert.rejects(read(response([enc.encode('{"status":"loading"}\n')]), () => {}),
    /before its result arrived/);
});

test('legacy JSON and HTTP error bodies retain their result', async () => {
  for (const status of [200, 401, 500]) {
    const body = { ok: status === 200, error: { message: 'session expired' } };
    const result = await read(Response.json(body, { status }), () => assert.fail('unexpected progress'));
    assert.deepEqual(result, body);
  }
});

test('final result without a trailing newline and a failed prefix remain explicit', async () => {
  const result = await read(response([enc.encode('{"result":{"ok":true,"prefix":{"parked":false,"error":"cancelled"}}}')]), () => {});
  assert.equal(result.prefix.parked, false);
  assert.equal(result.prefix.error, 'cancelled');
});

test('the displayed progress distinguishes a loaded model from prompt preparation', () => {
  assert.equal(ctx.warmupStatus('prefilling 12 of 2400 prompt tokens'),
    'Model loaded · preparing chat: 12 / 2400 tokens');
  assert.equal(ctx.warmupStatus('loading'), 'loading');
});

// mm36: the pill's label for a warm-up that is WAITING on another request's
// warm-up of the same prefix (the server's "shared prefix tokens" status)
const wsBegin = html.indexOf('function warmupStatus(');
const wsEnd = html.indexOf('\nfunction pill(', wsBegin);
const wsCtx = vm.createContext({});
vm.runInContext(html.slice(wsBegin, wsEnd), wsCtx);
const warmupStatus = wsCtx.warmupStatus;

test('the shared-warm-up wait has its own label, and own progress keeps its label', () => {
  assert.equal(warmupStatus('prefilling 512 of 2453 shared prefix tokens (another request is preparing them)'),
    'Model loaded · sharing a warm-up in progress: 512 / 2453 tokens');
  assert.equal(warmupStatus('prefilling 512 of 2453 prompt tokens'),
    'Model loaded · preparing chat: 512 / 2453 tokens');
  assert.equal(warmupStatus('loading'), 'loading');
});


test('warmup mirrors the chat Loop switch only when tools are enabled', () => {
  const start = html.indexOf('function switchesForWarm()');
  const stop = html.indexOf('function toolsField()', start);
  const state = {webMode:'auto', loopOn:true};
  let toolsOn = true;
  const scope = vm.createContext({state, toolsField:()=>({off:[]}), anyToolsOn:()=>toolsOn});
  vm.runInContext(html.slice(start, stop), scope);
  assert.equal(scope.switchesForWarm().loop, true);
  state.loopOn = false;
  assert.equal(scope.switchesForWarm().loop, undefined);
  state.loopOn = true;
  toolsOn = false;
  assert.equal(scope.switchesForWarm().loop, undefined);
});

test('selected warmup uses current credentials without putting them in the URL', async () => {
  const start = html.indexOf('async function warm()');
  const stop = html.indexOf('// ------------------------------------------------------------ boot screen', start);
  let captured;
  let signedIn = true;
  const scope = vm.createContext({
    state: {model:'test-model', target:'auto'}, Date, AbortSignal,
    pill:()=>{}, ICONS:{tick:'', alert:''}, warmPill:{},
    switchesForWarm:()=>({loop:true}),
    ssoHeaders:()=>signedIn ? {'x-api-key':'synthetic-test-token'} : {},
    fetch:async(url, options)=>{captured={url,options};return {ok:true};},
    readWarmupResponse:async()=>({ok:true,target:'gpu'}),
  });
  vm.runInContext(html.slice(start, stop), scope);
  await scope.warm();
  assert.equal(captured.options.headers['x-api-key'], 'synthetic-test-token');
  assert.equal(captured.url.includes('synthetic-test-token'), false);
  signedIn = false;
  await scope.warm();
  assert.equal(Object.keys(captured.options.headers).length, 0);
});
