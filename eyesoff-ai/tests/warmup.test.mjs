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
