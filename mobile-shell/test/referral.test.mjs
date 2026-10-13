// The shell's referral code (src/referral.mjs): read from a link, kept per install, reported once.
// node --test test/referral.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { referralFrom, referralCode, saveReferral, storedReferral, installId, reportReferral } from "../src/referral.mjs";

const memory = () => { const m = new Map(); return { getItem: (k) => (m.has(k) ? m.get(k) : null), setItem: (k, v) => m.set(k, String(v)), m }; };
const hex = () => "ab".repeat(16);

test("a link carries a code as ?r= or ?ref=; anything else is no code", () => {
  assert.equal(referralFrom("enclave://open?u=https://x.app.enclave.host&r=steven"), "steven");
  assert.equal(referralFrom("https://enclave.host/get-app?ref=ACME-2026"), "ACME-2026");
  assert.equal(referralFrom("enclave://open?r=has%20space"), null);
  assert.equal(referralFrom("enclave://open?u=x"), null);
  assert.equal(referralFrom("not a url"), null);
  assert.equal(referralCode(" a.b_c-d "), "a.b_c-d");
});

test("a code can be corrected until it is reported; after that it is fixed", () => {
  const s = memory();
  assert.equal(saveReferral(s, "stevn"), true);
  assert.equal(saveReferral(s, "steven"), true);
  assert.deepEqual(storedReferral(s), { code: "steven", reported: false });
  assert.equal(saveReferral(s, "bad code"), false);
  s.setItem("enclave-shell-referral", JSON.stringify({ code: "steven", reported: true }));
  assert.equal(saveReferral(s, "other"), false);
  assert.equal(storedReferral(s).code, "steven");
});

test("reported once, with a stable install id; a failure is retried, a refusal is not", async () => {
  const s = memory(), sent = [];
  const post = async (url, body) => { sent.push({ url, body: JSON.parse(body) }); return { status: 200 }; };
  assert.equal(await reportReferral({ storage: s, post, apiBase: "https://api.enclave.host/", platform: "android", randomHex: hex }), "nothing");
  saveReferral(s, "steven");
  assert.equal(await reportReferral({ storage: s, post, apiBase: "https://api.enclave.host", platform: "web", randomHex: hex }), "skipped");
  assert.equal(await reportReferral({ storage: s, post, apiBase: "https://api.enclave.host/", platform: "android", app: "host.enclave.app", randomHex: hex }), "reported");
  assert.deepEqual(sent, [{ url: "https://api.enclave.host/v1/referrals",
    body: { installId: "ab".repeat(16), code: "steven", platform: "android", app: "host.enclave.app" } }]);
  assert.equal(await reportReferral({ storage: s, post, apiBase: "x", platform: "android", randomHex: hex }), "nothing", "once");
  assert.equal(installId(s, () => "cd".repeat(16)), "ab".repeat(16), "the id is made once");

  const f = memory(); saveReferral(f, "steven");
  const down = async () => { throw new Error("offline"); };
  assert.equal(await reportReferral({ storage: f, post: down, apiBase: "x", platform: "ios", randomHex: hex }), "failed");
  assert.equal(storedReferral(f).reported, false, "retried on a later launch");
  const refused = async () => ({ status: 400 });
  assert.equal(await reportReferral({ storage: f, post: refused, apiBase: "x", platform: "ios", randomHex: hex }), "failed");
  assert.equal(storedReferral(f).reported, true, "a code the relay refuses is not sent again every launch");
});
