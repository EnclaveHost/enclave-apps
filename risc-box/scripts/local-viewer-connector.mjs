import fs from 'node:fs';
import {spawn} from 'node:child_process';
const c = JSON.parse(fs.readFileSync(process.argv[2], 'utf8'));
const child = spawn(process.execPath, [c.verifier, c.url, ...c.args, '--forward', 'tcp:2222'], {stdio: 'inherit'});
for (const signal of ['SIGTERM', 'SIGINT']) process.on(signal, () => child.kill(signal));
child.on('error', e => {console.error(e.message); process.exitCode = 1;});
child.on('exit', code => {process.exitCode = code ?? 1;});
