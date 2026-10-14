import {test} from 'node:test';
import assert from 'node:assert/strict';
import {verifiedPin} from './refresh-local-viewer.mjs';
const lines = ['RESULT gate=open', 'RESULT doc_key_matches_handshake=1',
  'RESULT runtime_pinned=1', 'RESULT host_data_checked=1', 'RESULT tcb_checked=1',
  'RESULT key_stable_in_launch=1', 'RESULT second_nonce_verdict=attested',
  'RESULT replay_rejected=1', 'VERDICT attested reason="verified"',
  'RESULT spki_sha256=' + 'ab'.repeat(32)];
test('accept complete fresh verification; reject every missing assurance', () => {
  assert.equal(verifiedPin(lines.join('\n')), 'ab'.repeat(32));
  for (let i=0; i<lines.length; i++)
    assert.throws(() => verifiedPin(lines.filter((_, n) => n !== i).join('\n')));
  assert.throws(() => verifiedPin([...lines, lines.at(-1)].join('\n')));
  assert.throws(() => verifiedPin(lines.join('\n').replace('VERDICT attested', 'VERDICT reject')));
});
