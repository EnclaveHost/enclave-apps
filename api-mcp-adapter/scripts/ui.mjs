// The page at /, driven in a real browser. scripts/ui.sh stands the server
// up and runs this; the import path and the chrome binary come from it,
// because playwright is not a dependency of this repo.
//
// Usage: node ui.mjs <base-url> <api-key>  (with PLAYWRIGHT_IMPORT set)
const { chromium } = await import(process.env.PLAYWRIGHT_IMPORT);

const BASE = process.argv[2];
const KEY = process.argv[3];
const USER = '0x00a329c0648769a73afac7f9381e08fb43dbea72';
const errors = [];
const browser = await chromium.launch(
  process.env.PLAYWRIGHT_CHROME ? { executablePath: process.env.PLAYWRIGHT_CHROME } : {},
);
const page = await browser.newPage();
page.on('console', m => { if (m.type() === 'error') errors.push(m.text()); });
page.on('pageerror', e => errors.push('pageerror: ' + e.message));

let failed = 0;
const ok = (cond, what) => {
  console.log((cond ? 'PASS: ' : 'FAIL: ') + what);
  if (!cond) failed++;
};

await page.goto(BASE + '/', { waitUntil: 'networkidle' });
ok((await page.title()).includes('api-mcp-adapter'), 'the page loads');
const banner = await page.textContent('#banner');
ok(/serves 11 tools over MCP/.test(banner), 'the banner counts the tools and names the endpoint');
ok(/7 groups/.test(banner), 'and the switches they sit under');
ok(await page.isVisible('#keycard'), 'a keyed deployment asks for the key');
ok(await page.$eval('#toolscard', e => e.hidden), 'and lists nothing until it is given');
ok((await page.$$eval('#pills .pill', els => els.map(e => e.textContent))).includes('keyed'), 'the status pills say so');

// connect with the key and a named user
await page.fill('#key', KEY);
await page.fill('#user', USER);
await page.click('#connect');
await page.waitForSelector('#toolscard:not([hidden])', { timeout: 10000 });
const names = await page.$$eval('#tools .nm', els => els.map(e => e.textContent));
ok(names.length === 11, `${names.length} tools listed`);
ok(names.some(n => n.startsWith('notes_read(name)')), 'the per-user tool appears once a user is named');
// serde_json sorts a JSON object's keys (this app and eyesoff-ai both), so a
// signature renders alphabetically; what matters is required vs optional
ok(names.includes('echo_get(flag?, name, x?)'), 'optional arguments are marked, required are not');
const tags = await page.textContent('#tools');
for (const t of ['per-user', 'returns a picture', 'takes attached pictures', 'read-only', 'destructive', 'format prompt']) {
  ok(tags.includes(t), `the "${t}" tag renders`);
}
ok((await page.textContent('#hidden')) === '', 'nothing is hidden from a named caller');

// the connect snippets
ok((await page.textContent('#snippet')).includes('"handshake": false'), 'the eyesoff-ai tab is the pasteable entry');
ok((await page.textContent('#snippet')).includes('$MCP_ADAPTER_API_KEY'), 'which references the key as a secret, not a literal');
await page.click('[data-tab="claude"]');
ok((await page.textContent('#snippet')).includes('claude mcp add --transport http'), 'the Claude Code tab');
await page.click('[data-tab="curl"]');
ok((await page.textContent('#snippet')).includes('tools/list'), 'the curl tab');

// run a call through the page
const run = async (name, args) => {
  await page.selectOption('#callname', name);
  await page.fill('#callargs', args);
  await page.click('#run');
};
await run('search', '{"q":"enclave"}');
await page.waitForFunction(() => document.querySelector('#out').textContent.includes('two hits'), { timeout: 15000 });
ok((await page.textContent('#out')).includes('two hits about enclave'), 'a tool call runs and shows its result');
ok((await page.$$('#out ol li')).length === 3, 'its citations render as a numbered list');

await run('generate_image', '{"prompt":"a cat"}');
await page.waitForSelector('#out img', { timeout: 20000 });
ok((await page.getAttribute('#out img', 'src')).startsWith('data:image/png;base64,'), 'a generated picture is shown inline');

await run('fail', '{}');
await page.waitForFunction(() => document.querySelector('#callerr').textContent.includes('HTTP 500'), { timeout: 15000 });
ok(true, 'a failing tool reports its error rather than going quiet');

await run('search', 'not json');
await page.waitForFunction(() => document.querySelector('#callerr').textContent.includes('not valid JSON'), { timeout: 5000 });
ok(true, 'bad arguments are caught before the request');

// forget clears the key
await page.click('#forget');
await page.waitForFunction(() => document.querySelector('#toolscard').hidden, { timeout: 5000 });
ok(await page.$eval('#connectcard', e => e.hidden), 'Forget clears the key and hides everything it unlocked');

ok(errors.length === 0, 'zero console errors' + (errors.length ? ': ' + errors.join(' | ') : ''));
await browser.close();
console.log(failed ? `\n${failed} FAILED` : '\nALL PASS');
process.exit(failed ? 1 : 0);
