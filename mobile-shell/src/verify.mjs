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
 * @param {(url: string) => Promise<any>} opts.getJson
 *        fetches CROSS-ORIGIN json; injected because the webview build routes
 *        this one call through native HTTP (the app origin sends no CORS
 *        headers yet) while node tests just use fetch.
 * @param {(stage: string) => void} [opts.onStage]
 */
export async function verifyApp({ url, attestationPath, repo, getJson, onStage = () => {} }) {
  onStage("fetching the app's attestation document");
  let doc;
  try {
    doc = await getJson(url + attestationPath);
  } catch (e) {
    return fail("unreachable", `could not fetch ${url}${attestationPath}: ${e.message || e}`);
  }

  const endpoint = doc?.source?.endpoint;
  if (typeof endpoint !== "string" || !/^https:\/\//.test(endpoint)) {
    return fail("no-endpoint", "the app's attestation document names no https attestation endpoint");
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
