import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

// Messages sent while an answer runs wait in chat.queue and go to the model as
// ONE user turn (queueTurn) - run from chat.html itself. The browser flow
// (queue, Send now, Stop, Edit, Remove, errors) needs a real page and a held
// /chat stream; this pins the turn the model reads.
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('function userTurn(');
const end = html.indexOf('\n}\n', html.indexOf('function queueTurn(')) + 3;
assert.ok(begin > 0 && end > begin, 'userTurn..queueTurn found in chat.html');
const ctx = vm.createContext({});
vm.runInContext(html.slice(begin, end) + '\nObject.assign(globalThis, { userTurn, queuedItem, queueTurn });', ctx);
const { queuedItem, queueTurn } = ctx;
const plain = (o) => JSON.parse(JSON.stringify(o));

test('queued messages become one user turn, in the order they were sent', () => {
  const m = plain(queueTurn([queuedItem('also run the tests'), queuedItem('and lint it')]));
  assert.deepEqual(m, { role: 'user', content: 'also run the tests\n\nand lint it' });
});

test('pictures and clips from every queued message come along', () => {
  const m = plain(queueTurn([
    queuedItem('look at this', ['data:image/png;base64,AA']),
    queuedItem('', ['data:image/png;base64,BB'], ['data:video/mp4;base64,CC']),
    queuedItem('and this'),
  ]));
  assert.equal(m.content, 'look at this\n\nand this', 'a picture-only message adds no blank paragraph');
  assert.deepEqual(m.images, ['data:image/png;base64,AA', 'data:image/png;base64,BB']);
  assert.deepEqual(m.videos, ['data:video/mp4;base64,CC']);
});

test('a single queued message is sent exactly as typed, with no empty fields', () => {
  assert.deepEqual(plain(queueTurn([queuedItem('use clang instead', [], [])])), { role: 'user', content: 'use clang instead' });
  assert.deepEqual(plain(queuedItem('x', undefined, [])), { text: 'x' });
});
