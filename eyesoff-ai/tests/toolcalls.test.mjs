import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

// What the model wrote before a tool call stays with that call (leadIn), run
// from chat.html itself.
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('function leadIn(');
const end = html.indexOf('\n}\n', begin) + 3;
assert.ok(begin > 0 && end > begin, 'leadIn found in chat.html');
const ctx = vm.createContext({});
vm.runInContext(html.slice(begin, end) + '\nglobalThis.leadIn = leadIn;', ctx);
const leadIn = ctx.leadIn;

test('thinking and a sentence before the call are kept whole', () => {
  const t = '<think>\nThe user wants the VM state. I will call vm_status.\n</think>\n\nLet me check the VM.\n';
  assert.equal(leadIn(t), t.trimEnd());
});

test('thinking alone is kept: it is why the call was made', () => {
  assert.equal(leadIn('<think>\nneeds the clock\n</think>\n\n'), '<think>\nneeds the clock\n</think>');
});

test('nothing but tags or whitespace is nothing to keep', () => {
  for (const t of ['', '   \n', '<think>\n', '<think>\n\n</think>\n\n', undefined, null])
    assert.equal(leadIn(t), undefined, JSON.stringify(t));
});

test('a call that leaked into the text is cut off, and what preceded it kept', () => {
  assert.equal(leadIn('Checking now.\n<tool_call>\n{"name":"vm_status"'), 'Checking now.');
  assert.equal(leadIn('<tool_call>\n{"name":"vm_status","arguments":{}}\n</tool_call>'), undefined);
});

test('an unclosed thought (a call made mid-reasoning) is kept', () => {
  assert.equal(leadIn('<think>\nI should look first'), '<think>\nI should look first');
});
