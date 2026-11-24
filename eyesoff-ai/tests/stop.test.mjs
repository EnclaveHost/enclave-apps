import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

// Stop mid-generation keeps the work (stoppedTurn), and the next request tells
// the model what the stopped turn did (stoppedAccount) - run from chat.html itself.
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('function stoppedTurn(');
const end = html.indexOf('\n}\n', html.indexOf('function stoppedAccount(')) + 3;
assert.ok(begin > 0 && end > begin, 'stoppedTurn/stoppedAccount found in chat.html');
const ctx = vm.createContext({});
vm.runInContext(html.slice(begin, end) + '\nObject.assign(globalThis, { stoppedTurn, stoppedAccount });', ctx);
const { stoppedTurn, stoppedAccount } = ctx;

const calls = () => [
  { name: 'vm_status', arguments: { id: 'a' }, before: '<think>\nneed the state\n</think>\n\nChecking the VM.', ok: true, ms: 800, result: '{"state":"running"}' },
  { name: 'vm_exec', arguments: { cmd: 'make' }, before: 'Building now.' },   // still running when stopped
];

test('a stop mid-loop keeps the turn even though the reply buffer is empty', () => {
  const m = stoppedTurn({ reply: '', toolCalls: calls(), agents: null, sources: null, genImage: null });
  assert.ok(m, 'kept, not handed back to the composer');
  assert.equal(m.role, 'assistant');
  assert.equal(m.meta.stopped, true);
  assert.equal(m.tools.length, 2);
  assert.equal(m.tools[0].ok, true, 'a finished call keeps its result');
  assert.equal(m.tools[0].stopped, undefined);
  assert.equal(m.tools[1].ok, false, 'the call cut off mid-run is no longer "running"');
  assert.equal(m.tools[1].stopped, true);
});

test('words after the last call stay with the turn', () => {
  const m = stoppedTurn({ reply: 'The build failed on step 3 because', toolCalls: calls() });
  assert.equal(m.content, 'The build failed on step 3 because');
});

test('nothing happened at all: null, so the question goes back to the composer', () => {
  for (const reply of ['', '<think>\n', '<think>\n\n</think>\n', undefined])
    assert.equal(stoppedTurn({ reply, toolCalls: null, agents: null, sources: [], genImage: null }), null, JSON.stringify(reply));
});

test('sources, a picture or a subagent alone are worth keeping', () => {
  assert.ok(stoppedTurn({ reply: '', sources: [{ url: 'https://example.com' }] }));
  assert.ok(stoppedTurn({ reply: '', genImage: { data_uri: 'data:image/png;base64,AA' } }));
  const ag = { id: 2, task: 'read the logs', text: 'Found 3 errors', tools: [{ name: 'logs', arguments: {} }], done: false };
  const m = stoppedTurn({ reply: '', agents: [ag] });
  assert.equal(m.agents[0].done, true);
  assert.equal(m.agents[0].stopped, true);
  assert.equal(m.agents[0].ok, false);
  assert.equal(m.agents[0].tools[0].stopped, true, "the subagent's open call is stopped too");
});

test('the next request says what the stopped turn did, without its thinking', () => {
  const m = stoppedTurn({ reply: 'Halfway there', toolCalls: calls() });
  const t = stoppedAccount(m);
  assert.match(t, /^Halfway there\n\n\[Stopped by the user before this turn finished\. What it did:\n/);
  assert.match(t, /Checking the VM\.\n- vm_status \{"id":"a"\} -> ok: \{"state":"running"\}/);
  assert.match(t, /Building now\.\n- vm_exec \{"cmd":"make"\} -> stopped before it returned/);
  assert.doesNotMatch(t, /need the state|<think>/, 'reasoning is not resent');
  assert.ok(t.endsWith(']'));
});

test('long results are cut, and the whole account is bounded', () => {
  const big = Array.from({ length: 40 }, (_, i) => ({ name: 'read', arguments: { i }, ok: true, result: 'x'.repeat(5000) }));
  const t = stoppedAccount({ content: '', tools: big, meta: { stopped: true } });
  assert.ok(t.length <= 6002, `bounded (${t.length})`);
  assert.match(t, /x{600}…/);
});

test('a stop with no calls still tells the model it was stopped', () => {
  assert.equal(stoppedAccount({ content: 'Partial answer', meta: { stopped: true } }),
    'Partial answer\n\n[Stopped by the user before this turn finished.]');
});
