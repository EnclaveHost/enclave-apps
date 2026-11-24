import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
import { createPublicKey, verify, webcrypto, randomBytes } from 'node:crypto';

// The Enclave-session helpers, run from chat.html itself: encoding, the
// x-enclave-session header, JSON-RPC answers, and the session-end signature
// checked the way the vault checks it. SESSION_LIVE=1 also drives the LIVE
// read-only MCP tools (session_status, and session_request with a throwaway
// key that nobody approves).
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('const MCP_URL = ');
const end = html.indexOf('\n// ---- session UI', begin);
assert.ok(begin > 0 && end > begin, 'session helpers found in chat.html');

const ctx = vm.createContext({ crypto: webcrypto, TextEncoder, btoa, atob, URL, fetch, AbortSignal });
vm.runInContext(html.slice(begin, end) + '\nObject.assign(globalThis, { MCP_URL, sessHex, sessPub, sessUsd });', ctx);

const LIVE = process.env.SESSION_LIVE === '1';
const OWNER = '0x0b2d009c0c9af05b12100d77f3c815fea822ee61';
const FUTURE = () => Date.now() + 3600e3;
const b64 = (s) => Buffer.from(s, 'base64url');

async function liveSession(extra = {}) {
  const k = await ctx.sessNewKey();
  return { v: 1, state: 'live', ...k, vault: '0xB794C4DD0e1C5E6B72281799F282BfdE7654345D',
    sid: '0x' + 'ab'.repeat(32), owner: OWNER, expiresAt: FUTURE(), ...extra };
}

test('the page script still compiles as a whole', () => {
  const m = /<script>\n"use strict";([\s\S]*)<\/script>\s*<\/body>/.exec(html);
  assert.ok(m, 'main script found');
  assert.doesNotThrow(() => new vm.Script(m[1]));
});

test('base64url: unpadded, URL-safe, round-trips every length', () => {
  for (let n = 0; n < 70; n++) {
    const bytes = new Uint8Array(randomBytes(n));
    const s = ctx.sessB64u(bytes);
    assert.doesNotMatch(s, /[=+/]/);
    assert.equal(s, Buffer.from(bytes).toString('base64url'));
    assert.deepEqual(Buffer.from(ctx.sessUnb64u(s)), Buffer.from(bytes));
  }
  assert.equal(ctx.sessHex(ctx.sessUnhex('0x00ff10')), '00ff10');
  assert.throws(() => ctx.sessUnhex('0xabc'));
});

test('the public point goes to the MCP tools as 0x-hex of the JWK coordinates', async () => {
  const k = await ctx.sessNewKey();
  assert.match(k.d, /^[A-Za-z0-9_-]{43}$/);
  const p = ctx.sessPub(k);
  assert.match(p.x, /^0x[0-9a-f]{64}$/);
  assert.equal(p.x, '0x' + b64(k.x).toString('hex'));
  assert.equal(p.y, '0x' + b64(k.y).toString('hex'));
  assert.equal(Object.keys(p).join(), 'x,y');      // never d
});

test('x-enclave-session: exactly {v,vault,sid,x,y,d}, base64url, only for a live unexpired session', async () => {
  const s = await liveSession();
  const h = ctx.sessHeaders({ session: s });
  assert.deepEqual(Object.keys(h), ['x-enclave-session']);
  const v = h['x-enclave-session'];
  assert.match(v, /^[A-Za-z0-9_-]+$/);
  const j = JSON.parse(b64(v).toString('utf8'));
  assert.deepEqual(Object.keys(j), ['v', 'vault', 'sid', 'x', 'y', 'd']);
  assert.deepEqual(j, { v: 1, vault: s.vault, sid: s.sid, x: s.x, y: s.y, d: s.d });

  const none = (session, now) => assert.deepEqual({ ...ctx.sessHeaders({ session }, now) }, {});
  none(undefined);
  none(null);
  assert.deepEqual({ ...ctx.sessHeaders(null) }, {});
  none({ ...s, state: 'wait' });
  none({ ...s, state: 'ended' });
  none(ctx.sessEndedRecord(s, 'you', '$1.00'));
  none({ ...s, expiresAt: Date.now() - 1 });
  none({ ...s, expiresAt: Date.now() + 2000 });       // dies on the way: not sent
  none({ ...s, expiresAt: undefined });
  none({ ...s, d: undefined });
  none({ ...s, sid: null });
  none({ ...s, vault: null });
  none(s, s.expiresAt);                                // at the expiry instant
});

test('an ended record keeps nothing to sign with', async () => {
  const r = ctx.sessEndedRecord(await liveSession({ label: 'eyesoff · chat' }), 'you', "$4.80 to the owner's vault");
  assert.equal(r.state, 'ended');
  assert.equal(r.refunded, 4.8);
  for (const k of ['x', 'y', 'd']) assert.equal(r[k], undefined);
  assert.equal(ctx.sessKeeps(r), false);
  assert.equal(ctx.sessState(r), 'ended');
});

test('session state: a live session past its expiry (or without one) has ended', () => {
  assert.equal(ctx.sessState(null), 'none');
  assert.equal(ctx.sessState({ state: 'bogus' }), 'none');
  assert.equal(ctx.sessState({ state: 'wait' }), 'wait');
  assert.equal(ctx.sessState({ state: 'live', expiresAt: FUTURE() }), 'live');
  assert.equal(ctx.sessState({ state: 'live', expiresAt: Date.now() - 1 }), 'ended');
  assert.equal(ctx.sessState({ state: 'live' }), 'ended');
  assert.equal(ctx.sessKeeps({ state: 'wait' }), true);
  assert.equal(ctx.sessKeeps({ state: 'live' }), true);
  assert.equal(ctx.sessKeeps(null), false);
});

test('JSON-RPC answers: structured, text-only, tool errors, protocol errors, event streams, garbage', () => {
  const rpc = (result) => JSON.stringify({ jsonrpc: '2.0', id: 1, result });
  const o = { link: 'https://enclave.host/grant#x', checkCode: 'ABCD1234' };
  assert.deepEqual({ ...ctx.mcpResult(rpc({ content: [{ type: 'text', text: '{}' }], structuredContent: o })) }, o);
  assert.deepEqual({ ...ctx.mcpResult(rpc({ content: [{ type: 'text', text: JSON.stringify(o) }] })) }, o);
  assert.throws(() => ctx.mcpResult(rpc({ content: [{ type: 'text', text: 'Error: budgetUsd must be 0..250 (the beta vault cap)' }], isError: true })),
    (e) => e.message === 'budgetUsd must be 0..250 (the beta vault cap)');
  assert.throws(() => ctx.mcpResult(JSON.stringify({ jsonrpc: '2.0', id: 1, error: { code: -32600, message: 'Invalid Request' } })),
    /Invalid Request/);
  const sse = 'event: message\ndata: ' + rpc({ structuredContent: o }) + '\n\n';
  assert.deepEqual({ ...ctx.mcpResult(sse) }, o);
  assert.throws(() => ctx.mcpResult('<html>Bad Gateway</html>', 502), /HTTP 502/);
  assert.throws(() => ctx.mcpResult('', 0), /did not answer/);
  assert.throws(() => ctx.mcpResult(rpc({ content: [{ type: 'text', text: 'plain words' }] })), /cannot read/);
});

test('session_request arguments: agent preset, short label, owner only when the account is a wallet', async () => {
  const pub = ctx.sessPub(await ctx.sessNewKey());
  const a = ctx.sessRequestArgs(pub, 'x'.repeat(80), 5, 24, OWNER);
  const plain = (o) => JSON.parse(JSON.stringify(o));   // both sides out of the vm's realm
  assert.deepEqual(plain(a), plain({ publicKey: pub, label: 'x'.repeat(60), preset: 'agent',
    budgetUsd: 5, expiresInHours: 24, owner: OWNER }));
  assert.equal(ctx.sessRequestArgs(pub, 'l', 5, 1, 'acct_0123abcd').owner, undefined);
  assert.equal(ctx.sessRequestArgs(pub, 'l', 5, 1, null).owner, undefined);
  const label = ctx.sessLabel(new Date('2026-10-07T09:41:00Z'));
  assert.ok(label.startsWith('eyesoff · chat of ') && label.length <= 60, label);
});

test('budget field: dollars and cents from 0 to 250', () => {
  for (const [v, want] of [['5', 5], ['0', 0], ['250', 250], ['7.5', 7.5], ['12.34', 12.34], ['.5', 0.5], ['5.', 5],
    ['250.01', null], ['251', null], ['-1', null], ['', null], ['abc', null], ['1e2', null], ['12.345', null], [' 3 ', 3]])
    assert.equal(ctx.sessBudget(v), want, JSON.stringify(v));
});

test('only an https enclave.host link is opened', () => {
  assert.equal(ctx.sessLink('https://enclave.host/grant#eyJ2'), 'https://enclave.host/grant#eyJ2');
  assert.ok(ctx.sessLink('https://staging.enclave.host/grant#a'));
  for (const bad of ['http://enclave.host/grant', 'https://enclave.host.evil.example/grant', 'https://evilenclave.host/',
    'javascript:alert(1)', '', null, undefined])
    assert.equal(ctx.sessLink(bad), null, String(bad));
});

test('owner status: balance, expiry, delegation, and what the relay says the session is', () => {
  // the shape session_status {owner} answered live on 2026-10-07
  const d = { owner: OWNER, vault: '0xB794C4DD0e1C5E6B72281799F282BfdE7654345D', vaultDeployed: true,
    delegation: { ledger: '0x606C', supported: true, granted: true },
    sessions: [
      { sid: '0x75D1', label: 'Browser · Chrome on Linux', live: false, balance: '$0.00', spent: '$0.00', expiresAt: '2026-10-07T20:43:10.000Z', revoked: true },
      { sid: '0x02a3', label: 'eyesoff · chat', live: true, balance: '$3.00', spent: '$0.25', expiresAt: '2026-10-07T21:05:27.000Z', revoked: false },
      { sid: '0x0bad', live: null, revoked: false },
      { sid: '0x0dead', live: false, revoked: false, balance: '$1.00', expiresAt: '2026-10-07T21:05:27.000Z' },
    ], held: [] };
  const s = { sid: '0x02A3' };
  assert.equal(ctx.sessApply(s, d), 'live');
  assert.equal(s.balance, 3);
  assert.equal(s.spent, 0.25);
  assert.equal(s.expiresAt, Date.parse('2026-10-07T21:05:27.000Z'));
  assert.equal(s.deleg, true);
  assert.equal(s.label, 'eyesoff · chat');
  assert.equal(ctx.sessApply({ sid: '0x75d1' }, d), 'revoked');
  assert.equal(ctx.sessApply({ sid: '0x0dead' }, d), 'ended');
  assert.equal(ctx.sessApply({ sid: '0x0bad' }, d), null);     // the relay could not read the chain
  const missing = { sid: '0xffff', balance: 9 };
  assert.equal(ctx.sessApply(missing, d), null);
  assert.equal(missing.balance, 9);
  const nodeleg = { sid: '0x02a3' };
  ctx.sessApply(nodeleg, { ...d, delegation: { supported: false, granted: false } });
  assert.equal(nodeleg.deleg, undefined);
  const notyet = { sid: '0x02a3' };
  ctx.sessApply(notyet, { ...d, delegation: { supported: true, granted: false } });
  assert.equal(notyet.deleg, false);
  assert.equal(ctx.sessMoney("$4.80 to the owner's vault"), 4.8);
  assert.equal(ctx.sessMoney('n/a'), null);
});

test('session_end signature: WebCrypto P-256 over SHA-256 of the digest, verified the way the vault does', async () => {
  const s = await liveSession();
  const pub = createPublicKey({ key: { kty: 'EC', crv: 'P-256', x: s.x, y: s.y }, format: 'jwk' });
  for (let i = 0; i < 16; i++) {
    const digest = randomBytes(32);
    const sig = await ctx.sessSign(s, '0x' + digest.toString('hex'));
    assert.match(sig, /^0x[0-9a-f]{128}$/);
    const raw = Buffer.from(sig.slice(2), 'hex');
    // exactly what the vault's P256_VERIFY(sha256(digest), r, s, x, y) accepts
    assert.equal(verify('sha256', digest, { key: pub, dsaEncoding: 'ieee-p1363' }, raw), true);
    const other = Buffer.from(digest); other[0] ^= 1;
    assert.equal(verify('sha256', other, { key: pub, dsaEncoding: 'ieee-p1363' }, raw), false);
  }
  await assert.rejects(ctx.sessSign(s, '0x' + '00'.repeat(31)), /32 bytes/);
  const stranger = await liveSession();
  const sig = Buffer.from((await ctx.sessSign(stranger, '0x' + '11'.repeat(32))).slice(2), 'hex');
  assert.equal(verify('sha256', Buffer.alloc(32, 0x11), { key: pub, dsaEncoding: 'ieee-p1363' }, sig), false);
});

test('the service worker never touches a POST /chat carrying the session header', async () => {
  const source = readFileSync(new URL('../src/sw.js', import.meta.url), 'utf8');
  const handlers = {};
  let fetched = 0, cached = 0;
  vm.runInNewContext(source, {
    URL, Request, Response, fetch: () => { fetched++; return Promise.resolve(new Response('')); },
    self: { registration: { scope: 'https://eyesoff.example/' }, addEventListener: (n, f) => { handlers[n] = f; } },
    caches: { open: async () => ({ put: async () => { cached++; }, match: async () => undefined }), match: async () => undefined },
  });
  let responded = false;
  handlers.fetch({
    request: { url: 'https://eyesoff.example/chat', method: 'POST', mode: 'cors',
               headers: new Headers({ 'x-enclave-session': 'abc' }) },
    respondWith: () => { responded = true; }, waitUntil: () => {},
  });
  assert.equal(responded, false);
  assert.equal(fetched + cached, 0);
});

test('LIVE: session_status and session_request against mcp.enclave.host (throwaway key, never approved)',
  { skip: !LIVE && 'set SESSION_LIVE=1' }, async () => {
  assert.equal(ctx.MCP_URL, 'https://mcp.enclave.host');
  const st = await ctx.mcpCall('session_status', { owner: OWNER });
  assert.match(st.vault, /^0x[0-9a-fA-F]{40}$/);
  assert.ok(Array.isArray(st.sessions));
  assert.equal(typeof st.delegation.granted, 'boolean');
  for (const row of st.sessions) {
    const s = { sid: row.sid };
    const got = ctx.sessApply(s, st);
    assert.ok(['live', 'ended', 'revoked', null].includes(got));
    if (got === 'live') assert.ok(s.expiresAt > 0 && typeof s.balance === 'number');
  }

  const k = await ctx.sessNewKey();
  const pub = ctx.sessPub(k);
  const r = await ctx.mcpCall('session_request',
    ctx.sessRequestArgs(pub, 'eyesoff · test key, never approved', 1, 1, OWNER));
  assert.ok(ctx.sessLink(r.link), r.link);
  assert.match(r.keyHash, /^0x[0-9a-f]{64}$/);
  assert.equal(r.checkCode, r.keyHash.slice(2, 10).toUpperCase());
  assert.match(r.vault, /^0x[0-9a-fA-F]{40}$/);
  assert.match(r.sid, /^0x[0-9a-f]{64}$/);
  assert.equal(r.policy.preset, 'agent');
  assert.equal(r.policy.budget, '$1.00');
  // the grant in the link carries the PUBLIC point and never the private scalar
  const grant = Buffer.from(new URL(r.link).hash.slice(1), 'base64url').toString('utf8');
  assert.ok(!grant.includes(k.d) && !grant.includes(b64(k.d).toString('hex')));

  const byKey = await ctx.mcpCall('session_status', { publicKey: pub });
  assert.equal(byKey.keyHash, r.keyHash);
  assert.equal(byKey.sessions.length, 0);

  // a passkey account names no owner: the vault and sid are only known once approved
  const anon = await ctx.mcpCall('session_request', ctx.sessRequestArgs(pub, 'eyesoff · test', 0, 1, 'acct_00'));
  assert.equal(anon.vault, null);
  assert.equal(anon.sid, null);
  assert.equal(anon.keyHash, r.keyHash);

  await assert.rejects(ctx.mcpCall('session_request', ctx.sessRequestArgs(pub, 'x', 999, 1)), /0\.\.250/);
});
