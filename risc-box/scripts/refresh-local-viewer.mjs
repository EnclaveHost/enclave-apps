// Refresh the local viewer after an isolated app VM changes its endpoint/key.
// Manager metadata locates the endpoint; the pinned independent verifier is
// the authority for identity. Never learn expected measurements from a host.
import fs from 'node:fs/promises';
import {execFile} from 'node:child_process';
import {promisify} from 'node:util';
import {pathToFileURL} from 'node:url';
const exec = promisify(execFile);

export function verifiedPin(output) {
  for (const line of ['RESULT gate=open', 'RESULT doc_key_matches_handshake=1',
    'RESULT runtime_pinned=1', 'RESULT host_data_checked=1', 'RESULT tcb_checked=1',
    'RESULT key_stable_in_launch=1', 'RESULT second_nonce_verdict=attested',
    'RESULT replay_rejected=1']) {
    if (!output.split('\n').includes(line)) throw new Error(`verifier missing ${line}`);
  }
  if (!/^VERDICT attested /m.test(output)) throw new Error('attestation refused');
  const pins = [...output.matchAll(/^RESULT spki_sha256=([a-f0-9]{64})$/gm)];
  if (pins.length !== 1) throw new Error('ambiguous transport key');
  return pins[0][1];
}

async function atomic(path, data) {
  await fs.writeFile(path + '.new', data, {mode: 0o600});
  await fs.rename(path + '.new', path);
}

async function main(configPath) {
  const c = JSON.parse(await fs.readFile(configPath, 'utf8'));
  const {GuestdControl, parseKey} = await import(pathToFileURL(c.controlClient));
  const control = new GuestdControl(c.manager, parseKey(await fs.readFile(c.pairKey, 'utf8')));
  const response = await control.request('GET', '/vms');
  if (response.status !== 200) throw new Error('manager inventory unavailable');
  const candidates = response.body.vms.filter(v => v.name === c.deployment && v.status === 'running');
  if (candidates.length !== 1) throw new Error('no unique running deployment');
  const vm = candidates[0];
  if (!Number.isInteger(vm.hostPort) || vm.hostPort < 1024 || vm.hostPort > 65535)
    throw new Error('invalid local endpoint');
  const desired = {id: vm.id, port: vm.hostPort, key: vm.transportKeySha256};
  let previous;
  try {previous = JSON.parse(await fs.readFile(c.state, 'utf8'));} catch {}
  if (JSON.stringify(previous) === JSON.stringify(desired)) return;

  // These arguments contain administrator-pinned measurement, app identity,
  // runtime, release, AMD trust chain, minimum TCB and deployment identity.
  // The only host-selected input is the local ciphertext endpoint.
  const url = `https://127.0.0.1:${vm.hostPort}`;
  const {stdout} = await exec(process.execPath, [c.verifier, url, ...c.verifyArgs],
    {timeout: 45000, maxBuffer: 1024 * 1024});
  const pin = verifiedPin(stdout);
  if (pin !== desired.key) throw new Error('verified endpoint differs from inventory; retry');
  await fs.mkdir(c.bridgeDropin, {recursive: true});
  await fs.mkdir(c.connectorDropin, {recursive: true});
  await atomic(c.bridgeDropin + '/route.conf', '[Service]\n' +
    `Environment=GS_APP_CONNECT_ADDR=127.0.0.1:${vm.hostPort}\n` +
    `Environment=GS_APP_SPKI_SHA256=${pin}\n`);
  // Launch through a fixed helper to avoid shell/systemd quoting of pinned args.
  await atomic(c.connectorRoute, JSON.stringify({url, verifier: c.verifier, args: c.verifyArgs}));
  await atomic(c.connectorDropin + '/route.conf', '[Service]\nExecStart=\n' +
    `ExecStart=${process.execPath} ${c.connectorHelper} ${c.connectorRoute}\n`);
  await exec('systemctl', ['--user', 'daemon-reload']);
  await exec('systemctl', ['--user', 'restart', 'enclave-risc-connector.service', 'enclave-risc-moonlight.service']);
  await atomic(c.state, JSON.stringify(desired));
  console.log(`Refreshed attested RISC viewer for ${vm.id}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href)
  main(process.argv[2]).catch(e => { console.error(e.message); process.exitCode = 1; });
