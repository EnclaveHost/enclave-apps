// Verify-before-render: the shell's reason to exist. The same check enclave.host's dashboard badge runs
// (site/js/core/verify.js verifyAppInBrowser), with Enclave's own verifier (verifier/web, vendored at build by
// vendor-verifier.mjs and pinned by sha256): THIS device draws the nonce, the server's report and the app guest's
// report are checked here against the vendor roots pinned in the bundle, and the app's report must bind the key its
// public TLS certificate carries.
//
// Which deployment: the shell asks the platform. A <label>.app.enclave.host origin names it, a custom domain resolves
// through the relay's public routing map (GET /v1/domains/map), and the deployment's record names the box it runs on;
// the box's tier (how the relay verified it at attach: snp, hv-node, avf) picks the verifier. These are the relay's
// statements, as the site's are: the verdict checks the deployment the platform routes this origin to.
//
// Honest scope: a pass is the verifier's "verified" or "limited". Unpinned it is at best "limited": the expected
// measurements are the relay's own prediction, not a build this device reproduced, and the verdict lists that as an
// omission. Nothing here is a third party's verifier.
import * as enclaveVerifier from "../vendor/enclave-verifier-web.mjs";

// the box tier -> the verifier export that checks ONE app on it (site/js/core/verify.js APP_VERIFIERS)
export const APP_VERIFIERS = { snp: "verifySnpAppWeb", "hv-node": "verifyShieldAppWeb", avf: "verifyPvmAppWeb" };
export const TIER_NAMES = { snp: "AMD SEV-SNP", "hv-node": "Enclave Shield", avf: "phone pVM" };
const ID = /^0x[0-9a-f]{64}$/;
const BOX = /^[A-Za-z0-9_-]{1,64}$/;

// the box a deployment runs on, from its attestation pointer (https://api.enclave.host/t/<box>/...)
export const boxOfEndpoint = (ep) => {
  try { return (new URL(ep).pathname.match(/^\/t\/([A-Za-z0-9_-]{1,64})\//) || [])[1] || null; } catch { return null; }
};

/**
 * @param {object} opts
 * @param {string} opts.url              bare app origin, e.g. https://eyesoff.ai
 * @param {string} [opts.deploymentId]   0x… on-chain id (a pairing link carries it)
 * @param {string} [opts.apiBase]        https://api.enclave.host
 * @param {typeof fetch} opts.fetchImpl  every request the check makes, the verifier's included. Injected: the webview
 *                                       routes them through native HTTP (the relay grants CORS to enclave.host only),
 *                                       node uses fetch. Called unbound, as the verifier does.
 * @param {(stage: string) => void} [opts.onStage]
 * @param {object} [opts.verifier]       the verifier module (tests inject one)
 */
export async function verifyApp({ url, deploymentId, apiBase = "https://api.enclave.host", fetchImpl, onStage = () => {},
                                  verifier = enclaveVerifier }) {
  const getJson = async (path) => {
    const r = await fetchImpl(apiBase + path, { credentials: "omit", headers: { accept: "application/json" } });
    if (r.status !== 200) throw new Error(`GET ${path}: HTTP ${r.status}`);
    return r.json();
  };
  let host;
  try { host = new URL(url).host.toLowerCase(); } catch { return fail("bad-target", `not an app origin: ${url}`); }

  // ---- 1. which deployment serves this origin, and on which box
  onStage("finding the deployment behind this app");
  let id = ID.test(String(deploymentId || "").toLowerCase()) ? deploymentId.toLowerCase() : null, rec;
  try {
    if (!id) {
      const m = host.match(/^([0-9a-f]{8})\.app\.enclave\.host$/);
      if (!m) {
        const map = await getJson("/v1/domains/map");
        const mapped = String(map?.domains?.[host] || "").toLowerCase();
        if (!ID.test(mapped)) return fail("no-deployment", `the platform routes no deployment to ${host}`);
        id = mapped;
      }
      rec = await getJson(`/v1/deployments/${id || "0x" + m[1]}`);
      id = String(rec?.id || "").toLowerCase();
      if (!ID.test(id)) return fail("no-deployment", `the platform has no deployment for ${host}`);
    } else {
      rec = await getJson(`/v1/deployments/${id}`);
    }
  } catch (e) {
    return fail("unreachable", `could not look up the deployment behind ${host}: ${e.message}`);
  }
  if (rec?.status && rec.status !== "running") return fail("not-running", `deployment ${id.slice(0, 10)}… is ${rec.status}`, { id });
  let box = BOX.test(String(rec?.enclave || "")) ? rec.enclave : null;
  try {
    if (!box) box = boxOfEndpoint((await getJson(`/v1/deployments/${id}/attestation`))?.verification?.attestationEndpoint);
  } catch (_) {}
  if (!box) return fail("no-box", `the platform names no host for deployment ${id.slice(0, 10)}…`, { id });

  // ---- 2. the box's tier, as the relay verified it at attach (never the box's own word)
  let mode;
  try {
    const rows = (await getJson("/enclaves"))?.enclaves || [];
    mode = rows.find((e) => e && e.name === box)?.mode || null;
  } catch (e) {
    return fail("unreachable", `could not read the host list: ${e.message}`, { id, box });
  }
  const fn = APP_VERIFIERS[mode];
  if (!fn || typeof verifier[fn] !== "function")
    return fail("unsupported", `${box} is ${mode ? `a ${mode} host` : "not attached"}; this app cannot verify an app there`, { id, box, mode });

  // ---- 3. the check itself, on this device
  onStage(`verifying the app's ${TIER_NAMES[mode]} enclave on ${box}`);
  let v;
  try {
    v = await verifier[fn]({ apiBase, name: box, deployment: id, fetchImpl });
  } catch (e) {
    return fail("verifier-error", e?.message || String(e), { id, box, mode });
  }
  const steps = {};
  for (const k of v.checkOrder || Object.keys(v.checks || {})) {
    const c = v.checks?.[k];
    steps[k] = c === true ? "pass" : c === false ? "fail" : "skipped";
  }
  const pass = v.status === "verified" || v.status === "limited";
  return {
    pass,
    status: v.status,
    reason: pass ? null : v.status,
    error: pass ? null : (v.reasons || []).filter((r) => !/^OMITTED /.test(r)).at(-1) || v.summary || v.status,
    summary: v.summary || null,
    steps,
    omissions: v.omissions || [],
    deployment: { id, box, mode },
  };
}

function fail(reason, error, deployment = {}) {
  return { pass: false, status: "error", reason, error, summary: null, steps: {}, omissions: [], deployment };
}
