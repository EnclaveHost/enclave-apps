// Step 1 of the site activation: the ONE message the owner signs, and the upload it authorizes.
//   message:   exactly "enclave-upload:<frozen wasm sha256>:<now + 300>" (the CLI's own format, cli/enclave.mjs pinBytes)
//   recovered: the signature must recover to the pinned admin before anything is sent
//   token:     POST <tokenUrl> {hash, expiry, signature} -> {token, address, expiry} (relay/api-relay.js upload-token)
//   upload:    POST <addUrl> with the frozen bytes STREAMED and hashed in flight (size and sha256 must match the pins)
//   verify:    the returned CID must be the precomputed one; then the gateway's CAR of that CID is fetched (bounded) and
//              must hold EXACTLY the blocks of the frozen file's own DAG (importFile, the release tool's kubo-identical
//              builder), every block hashed. Only then is the upload "verified".
// The token and the signature live only in this call's memory: they are never returned, journaled, logged or put in an
// error message. Every request (token, upload, CAR) refuses redirects, so neither can be forwarded to another host, and
// every failure leaves this module as an UploadRefusal whose text is fixed here: an exception from fetch may quote the
// request or the remote reply, so it is classified, never relayed.
import fs from "node:fs";
import { createHash } from "node:crypto";
import { Readable } from "node:stream";
import { recoverMessageAddress, toHex } from "./vendor/viem.mjs";
import { importFile, cidString, cidParse } from "./unixfs.mjs";

const lc = (a) => String(a ?? "").toLowerCase();
const sha = (b) => createHash("sha256").update(b).digest("hex");

/** A refusal whose message is fixed text from this module: never remote content, the token or the signature. */
export class UploadRefusal extends Error { constructor(m) { super(m); this.name = "UploadRefusal"; } }
const refuse = (m) => { throw new UploadRefusal(m); };
const failureOf = (e) => (e?.name === "TimeoutError" || e?.name === "AbortError" ? "timed out"
  : /redirect/i.test(String(e?.cause?.message ?? e?.message ?? "")) ? "redirect refused" : "request failed");
/** One network exchange: redirects are an error, and anything but our own refusal becomes "<what>: <fixed kind>". */
async function exchange(fetchFn, what, url, init, read) {
  try { return await read(await fetchFn(url, { ...init, redirect: "error" })); }
  catch (e) { if (e instanceof UploadRefusal) throw e; throw new UploadRefusal(`${what}: ${failureOf(e)}`); }
}

/** The exact message for `nowMs`. */
export function uploadMessage(t, nowMs) {
  const expiry = Math.floor(nowMs / 1000) + t.upload.expirySec;
  const message = `enclave-upload:${t.wasm.sha256}:${expiry}`;
  return { message, expiry, hexMessage: toHex(message) };
}

/** Only the exact message for this artifact, unexpired (with margin), recovering to the admin. -> true or throws. */
export async function checkUploadSignature(t, { message, expiry }, signature, nowMs) {
  if (!/^0x[0-9a-fA-F]{130}$/.test(String(signature || ""))) refuse("not a 65-byte signature");
  if (message !== `enclave-upload:${t.wasm.sha256}:${expiry}`) refuse("the message is not this artifact's upload authorization");
  if (Math.floor(nowMs / 1000) > expiry - t.upload.signMarginSec) refuse("the upload authorization expired; sign a fresh one");
  if (expiry > Math.floor(nowMs / 1000) + t.upload.expirySec + 5) refuse("the upload authorization expires too late");
  let who; try { who = await recoverMessageAddress({ message, signature }); } catch { refuse("the signature could not be recovered"); }
  if (lc(who) !== lc(t.admin)) refuse("the signature is not the admin's");
  return true;
}

/** Read a fetch Response body into memory, refusing more than `limit` bytes. */
export async function readBounded(res, limit, what) {
  const len = Number(res.headers?.get?.("content-length") ?? NaN);
  if (Number.isFinite(len) && len > limit) refuse(`${what}: response larger than ${limit} bytes`);
  const parts = []; let n = 0;
  // chunks are wrapped, not copied: a 46 MB CAR is held once (plus one concat when it arrives in pieces)
  for await (const chunk of res.body) { n += chunk.length; if (n > limit) refuse(`${what}: response larger than ${limit} bytes`); parts.push(Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk.buffer, chunk.byteOffset, chunk.byteLength)); }
  return parts.length === 1 ? parts[0] : Buffer.concat(parts);
}
const jsonOf = async (res, t, what) => {
  if (!res.ok) refuse(`${what}: HTTP ${Number(res.status)}`);
  const b = await readBounded(res, t.upload.maxJsonBytes, what);
  try { return JSON.parse(b.toString("utf8")); } catch { refuse(`${what}: not JSON`); }
};

/** The frozen bytes, checked against the pins (size and sha256). */
export function frozenWasm(t) {
  const st = fs.statSync(t.wasm.path);
  if (!st.isFile() || st.size !== t.wasm.bytes) refuse("the frozen wasm is not the pinned size");
  const bytes = fs.readFileSync(t.wasm.path);
  if (sha(bytes) !== t.wasm.sha256) refuse("the frozen wasm is not the pinned artifact");
  const dag = importFile(bytes);
  if (cidString(dag.cid) !== t.expectedCid) refuse("the frozen wasm does not import to the pinned CID");
  return { bytes, dag };
}

/** The pinned file, streamed; its bytes are counted and hashed as they leave, and a difference errors the stream. */
function streamFrozen(t) {
  const h = createHash("sha256"); let n = 0;
  async function* chunks() {
    for await (const c of fs.createReadStream(t.wasm.path, { highWaterMark: 1 << 20 })) { n += c.length; if (n > t.wasm.bytes) refuse("the artifact grew while uploading"); h.update(c); yield c; }
    if (n !== t.wasm.bytes || h.digest("hex") !== t.wasm.sha256) refuse("the artifact changed while uploading");
  }
  return Readable.toWeb(Readable.from(chunks()));
}

/** The canonical DAG-CBOR CARv1 header for exactly one root: {"roots": [<cid>], "version": 1}, length-first key order. */
export function carHeaderOf(cid) {
  const c = cidParse(cid);
  return Buffer.concat([Buffer.from([0xa2, 0x65]), Buffer.from("roots"), Buffer.from([0x81, 0xd8, 0x2a, 0x58, c.length + 1, 0]), c, Buffer.from([0x67]), Buffer.from("version"), Buffer.from([1])]);
}
const MAX_VARINT_BYTES = 5;   // 2^35: far above any section of a CAR bounded by maxCarBytes
/** An unsigned LEB128 at `i`, minimal and at most MAX_VARINT_BYTES long. -> [value, next index] */
function varintAt(buf, i) {
  let v = 0, mul = 1;
  for (let k = 0; k < MAX_VARINT_BYTES; k++) {
    if (i + k >= buf.length) refuse("truncated CAR varint");
    const b = buf[i + k]; v += (b & 127) * mul;
    if (!(b & 128)) { if (k > 0 && b === 0) refuse("non-minimal CAR varint"); return [v, i + k + 1]; }
    mul *= 128;
  }
  refuse("oversized CAR varint");
}

/** A CARv1 whose whole header is the canonical one-root header, every block hashed, the exact block set of the frozen DAG. -> { carSha256, blocks } */
export function verifyCar(t, car, dag) {
  const header = carHeaderOf(t.expectedCid);
  const [hlen, h0] = varintAt(car, 0);
  if (hlen !== header.length || !car.subarray(h0, h0 + hlen).equals(header)) refuse("the CAR header is not exactly {roots: [the expected CID], version: 1}");
  let i = h0 + hlen;
  const want = new Map([...dag.blocks].map(([c, b]) => [cidString(c), b]));
  const seen = new Set();
  while (i < car.length) {
    const [len, at] = varintAt(car, i), end = at + len;
    if (len < 36 || end > car.length) refuse("truncated CAR block");
    const c = car.subarray(at, at + 36), data = car.subarray(at + 36, end), k = cidString(c);
    if (!createHash("sha256").update(data).digest().equals(c.subarray(4))) refuse("a CAR block does not hash to its CID");
    if (!want.has(k)) refuse("the CAR carries a block that is not in the frozen file's DAG");
    if (seen.has(k)) refuse("a CAR block appears twice");
    seen.add(k); i = end;
  }
  if (seen.size !== want.size) refuse(`the CAR lacks ${want.size - seen.size} of the frozen file's blocks`);
  return { carSha256: sha(car), blocks: seen.size };
}

/**
 * runUpload({ t, auth: {message, expiry}, signature, fetchFn, nowMs }) -> { cid, carSha256, blocks, verifiedAt }
 * Throws only UploadRefusal (fixed text: never the token, the signature or remote content).
 */
export async function runUpload({ t, auth, signature, fetchFn, nowMs = Date.now() }) {
  await checkUploadSignature(t, auth, signature, nowMs);
  const { dag } = frozenWasm(t);
  const signal = AbortSignal.timeout(t.upload.timeoutMs);
  const tok = await exchange(fetchFn, "upload token", t.upload.tokenUrl, { method: "POST", headers: { "content-type": "application/json" }, signal,
    body: JSON.stringify({ hash: t.wasm.sha256, expiry: auth.expiry, signature }) }, (res) => jsonOf(res, t, "upload token"));
  if (typeof tok?.token !== "string" || tok.token.length < 16 || tok.token.length > 512) refuse("upload token: malformed reply");
  if (lc(tok.address) !== lc(t.admin) || Number(tok.expiry) !== auth.expiry) refuse("upload token: issued for another address or expiry");
  const added = await exchange(fetchFn, "upload", t.upload.addUrl, { method: "POST", duplex: "half", signal, body: streamFrozen(t),
    headers: { "content-type": "application/wasm", "content-length": String(t.wasm.bytes), "x-upload-address": lc(t.admin), "x-upload-expiry": String(auth.expiry), "x-upload-token": tok.token } }, (res) => jsonOf(res, t, "upload"));
  if (added?.cid !== t.expectedCid) refuse(`upload: the gateway returned a CID other than the expected ${t.expectedCid}`);
  if (added.wasi !== undefined && added.wasi !== "0.2") refuse("upload: the gateway classified another world");
  const car = await exchange(fetchFn, "CAR fetch", t.upload.carUrl(t.expectedCid), { method: "GET", signal }, (res) => {
    if (!res.ok) refuse(`CAR fetch: HTTP ${Number(res.status)}`);
    return readBounded(res, t.upload.maxCarBytes, "CAR fetch");
  });
  return { cid: t.expectedCid, ...verifyCar(t, car, dag), verifiedAt: new Date(nowMs).toISOString() };
}
