// Repackage only the embedded page in the exact published SET64/AOT component.
// Keeping its byte range fixed preserves every code section, offset, import,
// memory setting and baked optimization. Publish the result as a NEW version;
// never replace an existing content-addressed artifact or bypass attestation.
const fs = require('node:fs');
const crypto = require('node:crypto');
const {transformSync} = require('esbuild');
const [original, oldPage, newPage, output] = process.argv.slice(2);
if (!output) throw Error('usage: package-ui.cjs original.wasm original.html updated.html new.wasm');
const wasm = fs.readFileSync(original), before = fs.readFileSync(oldPage);
const sha = bytes => crypto.createHash('sha256').update(bytes).digest('hex');
if (sha(wasm) !== '533692386cc170907bad7299f998b1bcf797d00469abb15ed5f02b5686666d18')
  throw Error('not the exact published 0.6.54 component');
const offset = wasm.indexOf(before);
if (offset < 0 || wasm.indexOf(before, offset + 1) !== -1) throw Error('page must match exactly once');
let page = fs.readFileSync(newPage, 'utf8');
page = page.replace(/<script>([\s\S]*?)<\/script>/g, (_, code) =>
  '<script>' + transformSync(code, {loader:'js', target:'es2020', minify:true}).code.replace(/<\/script/gi, '<\\/script') + '</script>');
const bytes = Buffer.from(page);
if (bytes.length > before.length) throw Error('new page does not fit the existing data range');
const packed = Buffer.alloc(before.length, 32); bytes.copy(packed);
const result = Buffer.from(wasm); packed.copy(result, offset);
if (!result.subarray(0, offset).equals(wasm.subarray(0, offset)) ||
    !result.subarray(offset + before.length).equals(wasm.subarray(offset + before.length)))
  throw Error('bytes outside the HTML changed');
fs.writeFileSync(output, result, {flag:'wx'});
fs.writeFileSync(output + '.html', packed, {flag:'wx'});
console.log(JSON.stringify({sha256:sha(result), originalSha256:sha(wasm), bytes:result.length,
  htmlOffset:offset, htmlRangeBytes:before.length, minifiedHtmlBytes:bytes.length}));
