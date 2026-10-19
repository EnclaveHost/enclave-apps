// The native-bridge fetch (src/native-fetch.mjs) against answers shaped as Android's CapacitorHttp gives them.
// node --test test/native-fetch.test.mjs
import test from "node:test";
import assert from "node:assert/strict";
import { makeNativeFetch } from "../src/native-fetch.mjs";

const bridge = (answer, seen = []) => ({ request: async (o) => { seen.push(o); return typeof answer === "function" ? answer(o) : answer; } });

test("a binary body arrives as base64 and leaves as the same bytes, streamable", async () => {
  const der = Uint8Array.from([0x30, 0x82, 0x05, 0x00, 0xff, 0x00, 0x7f]);
  const f = makeNativeFetch(bridge({ status: 200, headers: { "Content-Type": "application/octet-stream" }, data: Buffer.from(der).toString("base64") }));
  const r = await f("https://api.enclave.host/v1/amd-kds/vcek/v1/Turin/crl", { credentials: "omit" });
  assert.equal(r.ok, true);
  const reader = r.body.getReader(), parts = [];
  for (;;) { const { done, value } = await reader.read(); if (done) break; parts.push(...value); }
  assert.deepEqual(Uint8Array.from(parts), der);
});

test("text under arraybuffer is base64 too; JSON arrives parsed and reads back as JSON", async () => {
  const pem = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
  const t = makeNativeFetch(bridge({ status: 200, headers: { "content-type": "application/x-pem-file" }, data: Buffer.from(pem).toString("base64") }));
  assert.equal(await (await t("https://x/chain")).text(), pem);
  const j = makeNativeFetch(bridge({ status: 200, headers: { "content-type": "application/json; charset=utf-8" }, data: { mode: "snp", nonce: "ab" } }));
  assert.deepEqual(await (await j("https://x/a")).json(), { mode: "snp", nonce: "ab" });
});

test("an error body is its text or parsed JSON, with the status kept", async () => {
  const busy = makeNativeFetch(bridge({ status: 503, error: true, headers: { "content-type": "application/json" }, data: { error: "busy", retryAfter: 30 } }));
  const r = await busy("https://x/a");
  assert.equal(r.status, 503);
  assert.deepEqual(await r.json(), { error: "busy", retryAfter: 30 });
  const nf = makeNativeFetch(bridge({ status: 404, error: true, headers: { "content-type": "text/plain" }, data: "not found" }));
  assert.equal(await (await nf("https://x/b")).text(), "not found");
});

test("a JSON POST body goes over as an object; an abort rejects as AbortError", async () => {
  const seen = [];
  const f = makeNativeFetch(bridge({ status: 200, headers: { "content-type": "application/json" }, data: {} }, seen));
  await f("https://x/c", { method: "POST", body: JSON.stringify({ nonce: "ab" }), headers: { "content-type": "application/json" } });
  assert.deepEqual(seen[0].data, { nonce: "ab" });
  assert.equal(seen[0].method, "POST");
  const hang = makeNativeFetch({ request: () => new Promise(() => {}) });
  const ctrl = new AbortController(); setTimeout(() => ctrl.abort(), 10);
  await assert.rejects(hang("https://x/d", { signal: ctrl.signal }), (e) => e.name === "AbortError");
});
