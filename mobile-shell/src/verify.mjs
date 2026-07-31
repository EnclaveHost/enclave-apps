// Verify-before-render: the shell's reason to exist. Mirrors the CLI's
// attestation flow (cli/enclave.mjs verifyEnclaveOrigin): fetch the app's own
// /attestation to learn WHICH enclave origin to check, then run
// @tinfoilsh/verifier against that origin - quote to vendor root, Sigstore
// provenance for the pinned repo's release, measurement match, TLS binding.
//
// The repo is PINNED by the shell build and never read from the response: a
// response that could choose the repo could choose one whose attacker-built
// release the quote matches.
//
// Honest scope (matches the site and CLI today): a PASS proves the deployment
// this app names runs the signed release inside real enclave hardware, with
// TLS into that enclave origin. The app-domain leg itself terminates at the
// platform relay until in-enclave app TLS ships, so the copy in the splash
// says "deployment verified", never "this connection is attested".
import { Verifier } from "@tinfoilsh/verifier";

/**
 * @param {object} opts
 * @param {string} opts.url             bare app origin, e.g. https://eyesoff.ai
 * @param {string} opts.attestationPath usually /attestation
 * @param {string} opts.repo            pinned GitHub repo, exact casing
 * @param {string} [opts.deploymentId]  0x… on-chain id, for the API fallback
 * @param {string} [opts.apiBase]       https://api.enclave.host
 * @param {(url: string) => Promise<any>} opts.getJson
 *        fetches CROSS-ORIGIN json; injected because the webview build routes
 *        this one call through native HTTP (the app origin sends no CORS
 *        headers yet) while node tests just use fetch.
 * @param {(stage: string) => void} [opts.onStage]
 */
export async function verifyApp({ url, attestationPath, repo, deploymentId, apiBase, getJson, onStage = () => {} }) {
  onStage("fetching the app's attestation document");
  // The app's own /attestation is a llm-chat-style convention, not a platform
  // contract - a generic wrap can't assume it. The PLATFORM's per-deployment
  // attestation is public and exists for every deployment, so it is the
  // fallback (and, given a deployment id but no serving app, the primary):
  // same flow as `enclave attest`. An origin like <label>.app.enclave.host
  // even yields the id: the API resolves the 0x<label> prefix to the record.
  let doc = null, lastErr = null;
  try {
    doc = await getJson(url + attestationPath);
  } catch (e) {
    lastErr = e;
  }
  let endpoint = doc?.source?.endpoint;
  if ((typeof endpoint !== "string" || !/^https:\/\//.test(endpoint)) && apiBase) {
    let id = deploymentId;
    try {
      if (!id) {
        const m = new URL(url).host.match(/^([0-9a-f]{8})\.app\.enclave\.host$/i);
        if (m) id = (await getJson(`${apiBase}/v1/deployments/0x${m[1]}`))?.id;
      }
      if (id) {
        onStage("fetching the platform's attestation record");
        const att = await getJson(`${apiBase}/v1/deployments/${id}/attestation`);
        const ep = att?.verification?.attestationEndpoint;
        if (typeof ep === "string" && /^https:\/\//.test(ep)) {
          endpoint = ep;
          doc = doc || { deployment: { id } };
        }
      }
    } catch (e) {
      lastErr = lastErr || e;
    }
  }
  if (typeof endpoint !== "string" || !/^https:\/\//.test(endpoint)) {
    return fail(
      doc || lastErr === null ? "no-endpoint" : "unreachable",
      doc ? "no attestation endpoint found for this app"
          : `could not fetch an attestation for ${url}: ${lastErr?.message || lastErr}`
    );
  }
  const origin = new URL(endpoint).origin;

  onStage(`verifying ${origin} against the ${repo} release`);
  const v = new Verifier({ serverURL: origin, configRepo: repo });
  let failure = null;
  try {
    await v.verify();
  } catch (e) {
    failure = e;
  }
  const vdoc = v.getVerificationDocument();
  if (!vdoc) return fail("no-document", failure?.message || "the verifier produced no document");

  const word = (s) => (!s || s.status === "pending" ? "skipped" : s.status === "success" ? "pass" : "fail");
  const steps = {};
  for (const k of ["fetchDigest", "verifyEnclave", "verifyCode", "compareMeasurements", "verifyCertificate"]) {
    steps[k] = word(vdoc.steps?.[k]) + (vdoc.steps?.[k]?.error ? `: ${vdoc.steps[k].error}` : "");
  }
  return {
    pass: !!vdoc.securityVerified,
    reason: vdoc.securityVerified ? null : "verifier-failed",
    error: failure?.message || null,
    steps,
    origin,
    release: vdoc.releaseDigest ? `sha256:${vdoc.releaseDigest}` : null,
    measurement: vdoc.enclaveFingerprint || null,
    deployment: {
      id: doc?.deployment?.id || null,
      label: doc?.deployment?.label || null,
      enclave: doc?.deployment?.enclave || null,
      image: doc?.deployment?.image || null,
    },
  };
}

function fail(reason, error) {
  return { pass: false, reason, error, steps: {}, origin: null, release: null, measurement: null, deployment: {} };
}
