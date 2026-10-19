// The shell's verify flow (src/verify.mjs) with a stub verifier and a stub relay: which deployment, which host, which
// verifier, and what counts as a pass. node --test test/verify.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { verifyApp, boxOfEndpoint } from "../src/verify.mjs";

const ID = "0x" + "9eb4e600" + "ab".repeat(28);
const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });
function relay(routes) {
  const seen = [];
  const fetchImpl = async (url) => {
    const path = new URL(url).pathname + new URL(url).search;
    seen.push(path);
    const r = routes[path];
    return r === undefined ? json({ error: "not_found" }, 404) : typeof r === "function" ? r() : json(r);
  };
  return { fetchImpl, seen };
}
const ROUTES = {
  "/v1/domains/map": { domains: { "eyesoff.ai": ID } },
  [`/v1/deployments/${ID}`]: { id: ID, status: "running", enclave: "metal0" },
  "/v1/deployments/0x9eb4e600": { id: ID, status: "running", enclave: "metal0" },
  "/enclaves": { enclaves: [{ name: "metal0", mode: "snp" }, { name: "nucbox-k11-4", mode: "hv-node" }] },
};
const verdict = (status, checks = { server: true, binding: true }) => ({ status, checkOrder: Object.keys(checks), checks,
  omissions: status === "limited" ? ["expected-guest-relay-predicted"] : [], reasons: status === "rejected" ? ["the relay answered for another nonce"] : [], summary: `a ${status} verdict` });
const stubVerifier = (status, calls = []) => ({
  verifySnpAppWeb: async (o) => { calls.push(["snp", o.name, o.deployment]); return verdict(status); },
  verifyShieldAppWeb: async (o) => { calls.push(["shield", o.name, o.deployment]); return verdict(status); },
});

test("a custom domain resolves through the routing map; the SNP host gets the SNP app check", async () => {
  const { fetchImpl, seen } = relay(ROUTES), calls = [];
  const r = await verifyApp({ url: "https://eyesoff.ai", fetchImpl, verifier: stubVerifier("limited", calls) });
  assert.equal(r.pass, true);
  assert.equal(r.status, "limited");
  assert.deepEqual(r.deployment, { id: ID, box: "metal0", mode: "snp" });
  assert.deepEqual(calls, [["snp", "metal0", ID]]);
  assert.deepEqual(r.steps, { server: "pass", binding: "pass" });
  assert.deepEqual(r.omissions, ["expected-guest-relay-predicted"]);
  assert.ok(seen.includes("/v1/domains/map"));
});

test("a platform origin names its deployment by label; a pairing id skips the lookup", async () => {
  const a = relay(ROUTES);
  assert.equal((await verifyApp({ url: "https://9eb4e600.app.enclave.host", fetchImpl: a.fetchImpl, verifier: stubVerifier("verified") })).pass, true);
  assert.ok(a.seen.includes("/v1/deployments/0x9eb4e600") && !a.seen.includes("/v1/domains/map"));
  const b = relay(ROUTES);
  assert.equal((await verifyApp({ url: "https://9eb4e600.app.enclave.host", deploymentId: ID.toUpperCase().replace("0X", "0x"), fetchImpl: b.fetchImpl, verifier: stubVerifier("verified") })).pass, true);
  assert.ok(b.seen.includes(`/v1/deployments/${ID}`));
});

test("the box's tier picks the verifier; a host the shell cannot check fails closed", async () => {
  const calls = [];
  const shield = relay({ ...ROUTES, [`/v1/deployments/${ID}`]: { id: ID, status: "running", enclave: "nucbox-k11-4" } });
  assert.equal((await verifyApp({ url: "https://eyesoff.ai", fetchImpl: shield.fetchImpl, verifier: stubVerifier("limited", calls) })).pass, true);
  assert.deepEqual(calls, [["shield", "nucbox-k11-4", ID]]);
  const gone = relay({ ...ROUTES, "/enclaves": { enclaves: [] } });
  const r = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: gone.fetchImpl, verifier: stubVerifier("verified") });
  assert.equal(r.pass, false);
  assert.equal(r.reason, "unsupported");
});

test("only verified or limited is a pass; refusals, errors and lookups that fail are not", async () => {
  for (const s of ["rejected", "error"]) {
    const r = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: relay(ROUTES).fetchImpl, verifier: stubVerifier(s) });
    assert.equal(r.pass, false, s);
  }
  const rej = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: relay(ROUTES).fetchImpl, verifier: stubVerifier("rejected") });
  assert.equal(rej.error, "the relay answered for another nonce");
  const nomap = await verifyApp({ url: "https://other.example", fetchImpl: relay(ROUTES).fetchImpl, verifier: stubVerifier("verified") });
  assert.equal(nomap.reason, "no-deployment");
  const stopped = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: relay({ ...ROUTES, [`/v1/deployments/${ID}`]: { id: ID, status: "stopped" } }).fetchImpl, verifier: stubVerifier("verified") });
  assert.equal(stopped.reason, "not-running");
  const down = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: async () => { throw new Error("offline"); }, verifier: stubVerifier("verified") });
  assert.equal(down.reason, "unreachable");
  const thrown = await verifyApp({ url: "https://eyesoff.ai", fetchImpl: relay(ROUTES).fetchImpl, verifier: { verifySnpAppWeb: async () => { throw new Error("boom"); } } });
  assert.equal(thrown.pass, false);
});

test("the attestation pointer names the box when the record does not", async () => {
  assert.equal(boxOfEndpoint("https://api.enclave.host/t/metal0/.well-known/tinfoil-attestation"), "metal0");
  assert.equal(boxOfEndpoint("https://api.enclave.host/.well-known/x"), null);
  const { fetchImpl } = relay({ ...ROUTES, [`/v1/deployments/${ID}`]: { id: ID, status: "running" },
    [`/v1/deployments/${ID}/attestation`]: { verification: { attestationEndpoint: "https://api.enclave.host/t/metal0/.well-known/tinfoil-attestation" } } });
  const r = await verifyApp({ url: "https://eyesoff.ai", fetchImpl, verifier: stubVerifier("limited") });
  assert.equal(r.deployment.box, "metal0");
});
