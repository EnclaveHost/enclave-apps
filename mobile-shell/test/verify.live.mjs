// Live end-to-end proof of the shell's verify flow, in node where there is no
// CORS: the exact same verifyApp the splash bundles, against the real app in
// app.config.json. Network-dependent by nature - run it by hand or
// continue-on-error in CI, never as a merge gate.
import { readFileSync } from "node:fs";
import { verifyApp } from "../src/verify.mjs";

const app = JSON.parse(readFileSync(new URL("../app.config.json", import.meta.url), "utf8"));

const getJson = async (url) => {
  const r = await fetch(url, { headers: { accept: "application/json" } });
  if (!r.ok) throw new Error(`HTTP ${r.status}`);
  return r.json();
};

const t0 = Date.now();
const result = await verifyApp({
  url: app.url,
  attestationPath: app.attestationPath || "/attestation",
  repo: app.repo,
  getJson,
  onStage: (s) => console.log(`[stage] ${s}`),
});

console.log(JSON.stringify(result, null, 2));
console.log(`took ${((Date.now() - t0) / 1000).toFixed(1)}s`);
if (!result.pass) {
  console.error("VERIFY FAILED");
  process.exit(1);
}
console.log(`PASS: ${app.url} -> ${result.origin} runs ${result.release} in a verified enclave`);
