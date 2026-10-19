// Live end-to-end proof of the shell's verify flow, in node (no CORS): the exact verifyApp the splash bundles, with the
// vendored verifier (npm run vendor first), against the real app in apps/<APP>/app.json. Network-dependent by nature -
// run it by hand or continue-on-error in CI, never as a merge gate. It sends one fresh challenge to the app's host.
import { readFileSync } from "node:fs";
import { verifyApp } from "../src/verify.mjs";

const APP = process.env.APP || "eyesoff";
const app = JSON.parse(readFileSync(new URL(`../apps/${APP}/app.json`, import.meta.url), "utf8"));

const t0 = Date.now();
const result = await verifyApp({
  url: app.url,
  apiBase: "https://api.enclave.host",
  fetchImpl: (url, opts) => fetch(url, opts),
  onStage: (s) => console.log(`[stage] ${s}`),
});

console.log(JSON.stringify(result, null, 2));
console.log(`took ${((Date.now() - t0) / 1000).toFixed(1)}s`);
if (!result.pass) {
  console.error("VERIFY FAILED");
  process.exit(1);
}
