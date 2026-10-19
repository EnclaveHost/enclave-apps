// The shell verifies with Enclave's OWN browser verifier (EnclaveHost/enclave verifier/web, the bundle enclave.host serves
// at /vendor/enclave-verifier.js), fetched here at build time from the commit verifier.pin.json names and refused unless
// its sha256 is the pinned one. It is not committed: it is Enclave's software under Enclave's license, not this repo's
// MIT, and the pin is what decides which bytes ship. Its third-party notices ship beside the splash (www/).
//
//   node vendor-verifier.mjs            (ENCLAVE_VERIFIER_FILE=<path> uses a local copy, held to the same sha256)
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, mkdirSync } from "node:fs";

const pin = JSON.parse(readFileSync(new URL("./verifier.pin.json", import.meta.url), "utf8"));
const raw = (path) => `https://raw.githubusercontent.com/${pin.repo}/${pin.commit}/${path}`;
async function get(url) {
  const r = await fetch(url);
  if (!r.ok) throw new Error(`${url}: HTTP ${r.status}`);
  return Buffer.from(await r.arrayBuffer());
}

const local = process.env.ENCLAVE_VERIFIER_FILE;
const js = local ? readFileSync(local) : await get(raw(pin.file));
const sha = createHash("sha256").update(js).digest("hex");
if (sha !== pin.sha256) throw new Error(`the verifier bundle's sha256 is ${sha}, not the pinned ${pin.sha256}; refusing to build with it`);
mkdirSync(new URL("./vendor/", import.meta.url), { recursive: true });
writeFileSync(new URL("./vendor/enclave-verifier-web.mjs", import.meta.url), js);
if (!local) writeFileSync(new URL("./www/verifier-notices.md", import.meta.url), await get(raw(pin.notices)));
console.log(`verifier ${pin.repo}@${pin.commit.slice(0, 9)} sha256 ${sha.slice(0, 16)}… -> vendor/enclave-verifier-web.mjs`);
