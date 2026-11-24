import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

// Once an answer compacted its context (the server's `compact` tool), the next
// request starts at that answer's question with the summary in front of it -
// run from chat.html itself.
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('function compactSummary(');
const end = html.indexOf('\n}\n', html.indexOf('function sinceCompaction(')) + 3;
assert.ok(begin > 0 && end > begin, 'compactSummary/sinceCompaction found in chat.html');
const ctx = vm.createContext({});
vm.runInContext(html.slice(begin, end) + '\nObject.assign(globalThis, { compactSummary, sinceCompaction });', ctx);
const { compactSummary, sinceCompaction } = ctx;
const plain = (o) => JSON.parse(JSON.stringify(o));

const chat = (answer) => [
  { role: 'user', content: 'first question', images: ['data:image/png;base64,AA'] },
  { role: 'assistant', content: 'first answer' },
  { role: 'user', content: 'build the app' },
  answer,
  { role: 'user', content: 'now deploy it' },
];
const compacted = {
  role: 'assistant', content: 'Built.',
  tools: [
    { name: 'run', arguments: { cmd: 'make' }, ok: true },
    { name: 'compact', arguments: { summary: 'Goal: build the app. Done: make passes.' }, ok: true },
  ],
};

test('a chat that never compacted is sent whole', () => {
  const msgs = chat({ role: 'assistant', content: 'Built.', tools: [{ name: 'run', arguments: {}, ok: true }] });
  assert.equal(sinceCompaction(msgs), msgs);
});

test('after a compaction: from its question on, the summary in front', () => {
  const out = plain(sinceCompaction(chat(compacted)));
  assert.deepEqual(out.map((m) => m.role), ['user', 'assistant', 'user']);
  assert.match(out[0].content, /^\[Earlier in this conversation - your own summary[^\n]*\nGoal: build the app\. Done: make passes\.\n\n\[The message you were answering:\]\nbuild the app$/);
  assert.equal(out[1].content, 'Built.');
  assert.equal(out[2].content, 'now deploy it');
  assert.equal(out[0].images, undefined, 'the dropped turns take their pictures with them');
});

test('the newest compaction wins, and a refused one is not a compaction', () => {
  assert.equal(compactSummary({ role: 'assistant', tools: [{ name: 'compact', arguments: { summary: 'x' }, ok: false }] }), null);
  assert.equal(compactSummary({ role: 'assistant', tools: [{ name: 'compact', arguments: { summary: '  ' }, ok: true }] }), null);
  const two = { role: 'assistant', content: '', tools: [
    { name: 'compact', arguments: { summary: 'old' }, ok: true },
    { name: 'compact', arguments: { summary: 'new' }, ok: true },
  ] };
  assert.equal(compactSummary(two), 'new');
  // the app's own (model would not) comes on the finished answer's meta
  assert.equal(compactSummary({ role: 'assistant', content: '', meta: { compact: { summary: 'app' } }, tools: [] }), 'app');
  assert.equal(compactSummary({ role: 'user', content: 'x', tools: [{ name: 'compact', arguments: { summary: 'u' }, ok: true }] }), null);
});

test('a stopped turn that compacted keeps the summary from its call', () => {
  const stopped = { ...compacted, meta: { stopped: true } };
  assert.match(plain(sinceCompaction(chat(stopped)))[0].content, /Goal: build the app/);
});
