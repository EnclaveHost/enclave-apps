const {test} = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const http = require('node:http');
const {chromium} = require('playwright');

test('web display is opt-in and stopping releases polling, SSE and decoder', async () => {
  const root = path.resolve(__dirname, '../src');
  const active = new Set();
  const counts = {bands: 0, video: 0, console: 0, input: 0};
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://localhost');
    if (url.pathname === '/') {
      res.setHeader('content-type', 'text/html');
      res.end(fs.readFileSync(process.env.TEST_INDEX_HTML || path.join(root, 'index.html')));
    } else if (url.pathname.startsWith('/a/')) {
      res.setHeader('content-type', url.pathname.endsWith('.css') ? 'text/css' : 'text/javascript');
      res.end(fs.readFileSync(path.join(root, 'vendor', path.basename(url.pathname))));
    } else if (url.pathname === '/status') {
      res.setHeader('content-type', 'application/json');
      res.end(JSON.stringify({phase:'running', title:'Test machine', instret:10, mips:1,
        consoleBytes:0, endpoint:'test', bucket:'test', kernel:'test', fs:'test'}));
    } else if (url.pathname === '/fb.bands') {
      counts.bands++; active.add(res); res.on('close', () => active.delete(res));
      // Leave a real long-poll pending; stop must cancel its HTTP request.
    } else if (url.pathname === '/video' || url.pathname === '/console') {
      const video = url.pathname === '/video'; counts[video ? 'video' : 'console']++;
      res.setHeader('content-type', 'text/event-stream'); res.flushHeaders();
      if (video) {
        active.add(res); res.on('close', () => active.delete(res));
        res.write('event: codec\ndata: {"w":960,"h":600,"codec":"av01.0.04M.08"}\n\n');
      }
    } else {if (url.pathname === '/input') counts.input++; res.end('{}');}
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  let browser;
  try { browser = await chromium.launch({headless:true,
    ...(process.env.CHROME_BIN ? {executablePath:process.env.CHROME_BIN} : {})}); }
  catch (e) {server.close(); throw e;}
  const page = await browser.newPage();
  const errors = []; page.on('pageerror', e => errors.push(e.message));
  await page.addInitScript(() => {
    window.__decoders = [];
    window.VideoDecoder = class {
      constructor(callbacks) {this.callbacks = callbacks; this.state = 'unconfigured'; window.__decoders.push(this);}
      configure() {this.state = 'configured';}
      close() {this.state = 'closed';}
    };
  });
  const until = async predicate => {
    for (let i=0; i<100; i++) {if (await predicate()) return; await new Promise(r=>setTimeout(r,20));}
    throw Error('condition did not become true');
  };
  try {
    await page.goto(`http://127.0.0.1:${server.address().port}`);
    await page.locator('#dispcard').waitFor({state:'visible'});
    await new Promise(r=>setTimeout(r,1700)); // includes another status refresh
    assert.equal(counts.bands,0); assert.equal(counts.video,0);
    assert.equal(await page.locator('#fb').isVisible(),false);
    assert.equal(counts.console,1);
    await page.locator('.xterm-helper-textarea').pressSequentially('echo ok');
    await until(()=>counts.input>0);

    await page.getByRole('button',{name:'Start web stream'}).click();
    await until(()=>active.size===1);
    assert.equal(counts.bands,1);
    await page.getByRole('button',{name:'Stop web stream'}).click();
    await until(()=>active.size===0);
    await new Promise(r=>setTimeout(r,1700));
    assert.equal(counts.bands,1); // refresh must not silently restart it

    await page.getByRole('button',{name:'Start web stream'}).click();
    await until(()=>active.size===1);
    await page.locator('#av1toggle').check();
    await until(()=>counts.video===1 && active.size===1);
    await page.waitForFunction(()=>window.__decoders.length===1);
    await page.getByRole('button',{name:'Stop web stream'}).click();
    await until(()=>active.size===0);
    assert.equal(await page.evaluate(()=>window.__decoders[0].state),'closed');
    assert.equal(await page.evaluate(()=>{
      let closed=false;
      window.__decoders[0].callbacks.output({close(){closed=true;}});
      return closed;
    }),true); // late frame is released, never painted

    // A page reload always defaults off, even if the browser restores form state.
    await page.getByRole('button',{name:'Start web stream'}).click();
    await until(()=>counts.video===2);
    await page.reload();
    await page.locator('#dispcard').waitFor({state:'visible'});
    await until(()=>active.size===0);
    assert.equal(await page.getByRole('button',{name:'Start web stream'}).getAttribute('aria-pressed'),'false');
    assert.equal(counts.video,2);
    assert.deepEqual(errors,[]);
  } finally {
    await browser.close(); server.closeAllConnections(); await new Promise(r=>server.close(r));
  }
});
