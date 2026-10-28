// UnixFS import of a whole site directory, with no remote importer: the exact DAG kubo 0.42 builds for
// `ipfs add -r --hidden --cid-version 1` (its default profile): CIDv1, sha2-256, 256 KiB chunks, raw leaves, balanced
// files of 174 links per node (the profile of sdk/storage/unixfs.mjs), basic dag-pb directories with links sorted by
// name bytes, and dotfiles included. Anything that profile would build differently is REFUSED rather than guessed: a
// directory over kubo's HAMT sharding threshold, symlinks and special files, and names that UnixFS cannot carry.
import fs from "node:fs";
import path from "node:path";
import { createHash } from "node:crypto";

export const CHUNK_BYTES = 262144;
const FANOUT = 174;
const HAMT_THRESHOLD = 262144;   // kubo: a directory whose link-size estimate reaches 256 KiB becomes a HAMT shard
const RAW = 0x55, DAG_PB = 0x70;

const sha = (b) => createHash("sha256").update(b).digest();
const varint = (n) => { const out = []; do { out.push((n % 128) + (n >= 128 ? 128 : 0)); n = Math.floor(n / 128); } while (n); return Buffer.from(out); };
const uint = (field, n) => Buffer.concat([varint(field * 8), varint(n)]);
const bytesField = (field, b) => Buffer.concat([varint(field * 8 + 2), varint(b.length), b]);
export const cidBytes = (codec, digest) => Buffer.concat([Buffer.from([1, codec, 0x12, 32]), digest]);

const B32 = "abcdefghijklmnopqrstuvwxyz234567";
export function cidString(bytes) {
  let out = "b", acc = 0, bits = 0;
  for (const b of bytes) { acc = (acc << 8) | b; bits += 8; while (bits >= 5) { bits -= 5; out += B32[(acc >>> bits) & 31]; } }
  if (bits) out += B32[(acc << (5 - bits)) & 31];
  return out;
}
export function cidParse(s) {
  if (typeof s !== "string" || !/^b[a-z2-7]+$/.test(s)) throw new Error("a base32 CIDv1 is required");
  const out = []; let acc = 0, bits = 0;
  for (const ch of s.slice(1)) { acc = (acc << 5) | B32.indexOf(ch); bits += 5; if (bits >= 8) { bits -= 8; out.push((acc >>> bits) & 255); } }
  const b = Buffer.from(out);
  if (b.length !== 36 || b[0] !== 1 || ![RAW, DAG_PB].includes(b[1]) || b[2] !== 0x12 || b[3] !== 32) throw new Error("only CIDv1 raw or dag-pb sha2-256 CIDs are used here");
  if (cidString(b) !== s) throw new Error("non-canonical CID string");
  return b;
}

// dag-pb: links (Hash, Name, Tsize) precede Data; empty names are encoded explicitly, as go-codec-dagpb does.
function pbNode(links, data) {
  return Buffer.concat([...links.map((l) => bytesField(2, Buffer.concat([bytesField(1, l.cid), bytesField(2, Buffer.from(l.name, "utf8")), uint(3, l.tsize)]))),
    bytesField(1, data)]);
}
/** Decode a dag-pb block -> {links: [{cid: Buffer, name, tsize}], data: Buffer}. Strict: unknown fields refuse. */
export function decodePb(block) {
  const read = (buf, i) => { let n = 0, mul = 1, b; do { if (i >= buf.length) throw new Error("truncated varint"); b = buf[i++]; n += (b & 127) * mul; mul *= 128; } while (b & 128); return [n, i]; };
  const fields = (buf) => { const out = []; let i = 0; while (i < buf.length) { let key, v; [key, i] = read(buf, i); const f = key >>> 3, t = key & 7;
    if (t === 0) { [v, i] = read(buf, i); out.push([f, v]); } else if (t === 2) { let len; [len, i] = read(buf, i); if (i + len > buf.length) throw new Error("truncated field"); out.push([f, buf.subarray(i, i + len)]); i += len; }
    else throw new Error("unsupported dag-pb wire type"); } return out; };
  const links = [], node = fields(block); let data = null;
  for (const [f, v] of node) {
    if (f === 2) { const l = { cid: null, name: null, tsize: null };
      for (const [g, w] of fields(v)) { if (g === 1) l.cid = Buffer.from(w); else if (g === 2) l.name = Buffer.from(w).toString("utf8"); else if (g === 3) l.tsize = w; else throw new Error("unknown dag-pb link field"); }
      if (!l.cid) throw new Error("dag-pb link without a hash"); links.push(l); }
    else if (f === 1) data = Buffer.from(v);
    else throw new Error("unknown dag-pb node field");
  }
  return { links, data: data || Buffer.alloc(0) };
}
/** UnixFS Data of a dag-pb node -> {type, filesize, blocksizes}. Type 1 = directory, 2 = file. */
export function decodeUnixfs(data) {
  const out = { type: null, filesize: null, blocksizes: [] }; let i = 0;
  const read = () => { let n = 0, mul = 1, b; do { b = data[i++]; n += (b & 127) * mul; mul *= 128; } while (b & 128); return n; };
  while (i < data.length) { const key = read(), f = key >>> 3, t = key & 7;
    if (t !== 0) throw new Error("unsupported UnixFS field");
    const v = read(); if (f === 1) out.type = v; else if (f === 3) out.filesize = v; else if (f === 4) out.blocksizes.push(v); else throw new Error("unsupported UnixFS field " + f); }
  return out;
}

/** One file -> {cid: Buffer, tsize, blocks: [[cidBytes, bytes]]}, exactly kubo's balanced, raw-leaf layout. */
export function importFile(bytes) {
  const blocks = [], leaves = [];
  for (let off = 0; off < bytes.length || off === 0; off += CHUNK_BYTES) {
    const b = bytes.subarray(off, off + CHUNK_BYTES), c = cidBytes(RAW, sha(b));
    leaves.push({ cid: c, content: b.length, total: b.length }); blocks.push([c, b]);
    if (!bytes.length) break;
  }
  const level = (start, end, depth) => {
    if (!depth) return leaves[start];
    const children = [], group = FANOUT ** (depth - 1);
    for (let i = start; i < end; i += group) children.push(level(i, Math.min(end, i + group), depth - 1));
    const content = children.reduce((n, c) => n + c.content, 0);
    const data = Buffer.concat([uint(1, 2), uint(3, content), ...children.map((c) => uint(4, c.content))]);
    const block = pbNode(children.map((c) => ({ cid: c.cid, name: "", tsize: c.total })), data), c = cidBytes(DAG_PB, sha(block));
    blocks.push([c, block]);
    return { cid: c, content, total: block.length + children.reduce((n, x) => n + x.total, 0) };
  };
  let depth = 0, capacity = 1; while (capacity < leaves.length) { depth++; capacity *= FANOUT; }
  const top = level(0, leaves.length, depth);
  return { cid: top.cid, tsize: top.total, blocks };
}

const NAME_OK = (n) => n.length > 0 && n !== "." && n !== ".." && !/[\/\0]/.test(n);
/**
 * Import a directory tree. -> {cid (string), root: Buffer (the root block), blocks: Map<cidString, Buffer>,
 * files: [{path, bytes, sha256, cid}] sorted by path}. Every file is read once; no file outside `dir` is followed.
 */
export function importDirectory(dir) {
  const blocks = new Map(), files = [];
  const put = (c, b) => { const k = cidString(c); if (!blocks.has(k)) blocks.set(k, b); };
  const walk = (abs, rel) => {
    const st = fs.lstatSync(abs);
    if (st.isSymbolicLink()) throw new Error(`refusing a symlink in the release tree: ${rel || "."}`);
    if (st.isFile()) {
      const bytes = fs.readFileSync(abs), f = importFile(bytes);
      for (const [c, b] of f.blocks) put(c, b);
      files.push({ path: rel, bytes: bytes.length, sha256: createHash("sha256").update(bytes).digest("hex"), cid: cidString(f.cid) });
      return { cid: f.cid, tsize: f.tsize };
    }
    if (!st.isDirectory()) throw new Error(`refusing a special file in the release tree: ${rel}`);
    const names = fs.readdirSync(abs);
    for (const n of names) if (!NAME_OK(n) || Buffer.from(n, "utf8").toString("utf8") !== n) throw new Error(`refusing an unrepresentable name in ${rel || "."}: ${JSON.stringify(n)}`);
    names.sort((a, b) => Buffer.compare(Buffer.from(a, "utf8"), Buffer.from(b, "utf8")));
    const links = names.map((n) => ({ name: n, ...walk(path.join(abs, n), rel ? rel + "/" + n : n) }));
    const estimate = links.reduce((s, l) => s + Buffer.byteLength(l.name) + l.cid.length, 0);
    if (estimate >= HAMT_THRESHOLD) throw new Error(`directory ${rel || "."} would be HAMT-sharded by kubo (${links.length} entries); this importer refuses it`);
    const block = pbNode(links, Buffer.from([0x08, 0x01])), c = cidBytes(DAG_PB, sha(block));
    put(c, block);
    return { cid: c, tsize: block.length + links.reduce((s, l) => s + l.tsize, 0) };
  };
  const top = walk(path.resolve(dir), "");
  if (!fs.statSync(dir).isDirectory()) throw new Error("the release root must be a directory");
  files.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
  const cid = cidString(top.cid);
  return { cid, root: blocks.get(cid), blocks, files };
}
