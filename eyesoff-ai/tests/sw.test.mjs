import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../src/sw.js', import.meta.url), 'utf8');

function worker(fetch, scope = 'https://app.example/') {
  const handlers = {};
  const stored = new Map([[scope, new Response('cached shell')]]);
  const cache = {
    match: async (url) => stored.get(url)?.clone(),
    put: async (url, response) => { stored.set(url, response); },
  };
  vm.runInNewContext(source, {
    URL, Request, Response, fetch,
    self: { registration: { scope }, addEventListener: (name, fn) => { handlers[name] = fn; } },
    caches: { open: async () => cache, match: cache.match },
  });
  return {
    stored,
    async request(path, options = {}) {
      let response;
      const pending = [];
      handlers.fetch({
        request: { url: new URL(path, scope).href, method: 'GET', mode: 'navigate', ...options },
        respondWith: (value) => { response = value; },
        waitUntil: (value) => { pending.push(value); },
      });
      const result = await response;
      await Promise.all(pending);
      return result;
    },
  };
}

for (const status of [401, 403, 409, 502]) {
  test(`online navigation preserves HTTP ${status} instead of hiding it behind the shell`, async () => {
    const w = worker(async () => new Response('current platform response', { status }));
    const response = await w.request('c/existing-chat');
    assert.equal(response.status, status);
    assert.equal(await response.text(), 'current platform response');
    assert.equal(await w.stored.get('https://app.example/').text(), 'cached shell');
  });
}

test('successful navigation returns and caches the current app page', async () => {
  const w = worker(async () => new Response('current shell'));
  assert.equal(await (await w.request('c/chat')).text(), 'current shell');
  assert.equal(await w.stored.get('https://app.example/').text(), 'current shell');
});

test('offline navigation can still use the cached shell', async () => {
  const w = worker(async () => { throw new TypeError('offline'); });
  assert.equal(await (await w.request('c/chat')).text(), 'cached shell');
});

test('offline navigation without a shell reports a network error', async () => {
  const w = worker(async () => { throw new TypeError('offline'); });
  w.stored.clear();
  assert.equal((await w.request('c/chat')).type, 'error');
});

test('prefixed private deployments retain their complete navigation path', async () => {
  const scope = 'https://node.example/x/deployment/https/';
  let requested;
  const w = worker(async (req) => {
    requested = req.url;
    return new Response('sign in', { status: 401 });
  }, scope);
  assert.equal((await w.request('c/chat')).status, 401);
  assert.equal(requested, scope + 'c/chat');
});

test('API calls, streaming POSTs and out-of-scope pages remain untouched', async () => {
  const w = worker(() => { throw new Error('should not be intercepted'); });
  for (const path of ['models', 'warmup', 'v1/models', 'c/chat/deeper', 'https://elsewhere.example/']) {
    assert.equal(await w.request(path), undefined);
  }
  assert.equal(await w.request('chat', { method: 'POST' }), undefined);
});

test('static assets still use their cache', async () => {
  const w = worker(() => { throw new Error('cached asset must not fetch'); });
  w.stored.set('https://app.example/icon-192.png', new Response('icon'));
  assert.equal(await (await w.request('icon-192.png', { mode: 'no-cors' })).text(), 'icon');
});
