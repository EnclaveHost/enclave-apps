import { readFileSync } from 'node:fs';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';

// The pad bank chip's parsing and states, run from chat.html itself against
// stand-in elements.
const html = readFileSync(new URL('../src/chat.html', import.meta.url), 'utf8');
const begin = html.indexOf('const PAD_TAG = ');
const end = html.indexOf('\nasync function padPoll(', begin);
assert.ok(begin > 0 && end > begin, 'pad bank code found in chat.html');

function el(kids = {}) {
  const e = {
    className: '', textContent: '', title: '', style: {}, attrs: {}, kids,
    classList: { remove: (c) => { e.className = e.className.split(' ').filter((x) => x && x !== c).join(' '); } },
    setAttribute(k, v) { e.attrs[k] = v; },
    querySelector(sel) { return kids[sel]; },
  };
  return e;
}
function load() {
  const chip = el({ '.pad-cell i': el(), '.lbl': el(), '.pad-dir': el() });
  const meter = el(); meter.firstElementChild = el();
  const ids = { padPct: el(), padState: el(), padMeter: meter, padReady: el(), padSize: el(), padRate: el(), padTotals: el() };
  const clock = { now: 0 };
  const ctx = vm.createContext({ performance: { now: () => clock.now }, $: (id) => ids[id], padChip: chip });
  vm.runInContext(html.slice(begin, end) + '\nglobalThis.pad = pad;', ctx);
  return { ctx, chip, ids, clock };
}

const TAG = 1346454594, links = 2, groups = 150, slots = 34000, per = links * groups, capacity = per * slots;
function header({ ready, written = 0, drawn = 0, attached = links, failed = 0, minters = 0, quiet = 60000 }) {
  const v = [1, TAG, 1, links, attached, capacity, ready, slots, Math.floor(ready / per),
    written, drawn, 0, failed, minters, quiet, 2000, 12.6e6, 512 * 2 ** 30];
  while (v.length < 24) v.push(0);
  return v.join(',');
}

test('only the pads layout is read: no header, the profile layout or garbage show nothing', () => {
  const { ctx } = load();
  assert.equal(ctx.padParse(null), null);
  assert.equal(ctx.padParse(''), null);
  assert.equal(ctx.padParse([1, 2, 10, 5, 6, ...Array(19).fill(0)].join(',')), null);   // v1 profile: card count in [1]
  assert.equal(ctx.padParse(header({ ready: 5 }).replace(String(TAG), '7')), null);
  assert.equal(ctx.padParse(header({ ready: 5 }).replace(/,0$/, ',x')), null);
  assert.equal(ctx.padParse('1,' + TAG + ',1,2'), null);
  const s = ctx.padParse(header({ ready: per * 100, written: 9, drawn: 4 }));
  assert.equal(s.tokens, 100);
  assert.equal(s.room, slots);
  assert.equal(s.drawn, 4);
});

test('refilling, in use, full, off - and a restart starts the rates over', () => {
  const { ctx, chip, ids, clock } = load();
  const show = (h) => { clock.now += 3000; ctx.padRender(ctx.padParse(header(h))); };
  let ready = Math.round(capacity * 0.42), written = ready, drawn = 0;

  show({ ready, written, minters: 5 });
  assert.match(chip.className, /\bshow\b/);
  assert.match(chip.className, /\bfill\b/);
  assert.equal(chip.kids['.lbl'].textContent, '42%');
  ready += 24 * per * 3; written += 24 * per * 3;
  show({ ready, written, minters: 5 });
  assert.match(ids.padRate.textContent, /^minting 24 tokens' worth a second · full in about/);
  assert.equal(chip.kids['.pad-dir'].textContent, '↑');

  ready -= 400 * per * 3; drawn += 400 * per * 3;
  show({ ready, written, drawn, quiet: 0 });
  assert.match(chip.className, /\bdraw\b/);
  assert.equal(ids.padState.textContent, 'in use by a prompt');
  assert.equal(chip.kids['.pad-dir'].textContent, '↓');

  show({ ready, written, drawn, quiet: 500 });
  assert.equal(ids.padState.textContent, 'refills once the current request ends');

  show({ ready: capacity - per * 20, written: written + 1, drawn, minters: 1 });
  assert.equal(chip.kids['.lbl'].textContent, '100%');
  assert.equal(ids.padState.textContent, 'full');
  assert.doesNotMatch(chip.className, /\bfill\b/);

  // the engine restarted: counters went back to zero
  show({ ready: per * 10, written: per * 10, minters: 5 });
  assert.equal(ctx.pad.win.length, 1);
  assert.match(chip.className, /\bfill\b/);

  show({ ready: 0, written: per * 10, attached: 0, failed: 1 });
  assert.match(chip.className, /\boff\b/);
  assert.equal(chip.kids['.lbl'].textContent, 'off');
  assert.match(ids.padState.textContent, /^off after a disk error/);

  ctx.padRender(null);
  assert.doesNotMatch(chip.className, /\bshow\b/);
});
