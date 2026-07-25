#!/usr/bin/env node
// Local end-to-end run of the egress demo WITHOUT the platform: boots the REAL
// enclave-side egress front (egress.js) and the REAL source-binding relay
// (relay/egress-relay.js), then launches the built component under a
// phase-2 patched wasmtime exactly the way the wasm-manager would:
//
//   -S egress=<front>  +  ENCLAVE_EGRESS_CRED in the process env (guest-invisible)
//   NO -Sinherit-network — the guest has no raw network at all
//
// and finally curls the page on BOTH declared ports and prints it.
//
// Env:
//   ENCLAVE_REPO              path to the enclave checkout   (default: ../../enclave)
//   ENCLAVE_EGRESS_WASMTIME   phase-2 patched wasmtime   (default: wasmtime on PATH)
//
// Caveats vs the platform: this box has no routed /64, so the "identity" the
// front derives is a placeholder and the relay dials from the box's own
// address (EGRESS_ALLOW_V4=1, v4-only echo) — on a real enclave [1]==[2]==[3].

import http from "node:http";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { fileURLToPath, pathToFileURL } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const REPO = path.resolve(process.env.ENCLAVE_REPO || path.join(HERE, "..", "..", "enclave"));
const WASMTIME = process.env.ENCLAVE_EGRESS_WASMTIME || "wasmtime";
const WASM = path.join(HERE, "target", "wasm32-wasip2", "release", "network-test.wasm");
const ID = "dep_demo";
const TOKEN = "dev-relay-token";
const HTTP_ACTUAL = 18080, TCP_ACTUAL = 17777;

const { createEgress, egressToken } = await import(pathToFileURL(path.join(REPO, "egress.js")));
const SECRET = Buffer.from("network-test-dev-secret");

// The identity the front hands out. A real enclave derives this from the id
// under its routed /64; locally we use the box's global v6 if it has one
// (then the relay can actually source-bind it), else a placeholder.
const globalV6 = Object.values(os.networkInterfaces()).flat()
  .find((i) => i && i.family === "IPv6" && !i.internal && i.scopeid === 0
        && !/^(fe80|fc|fd)/i.test(i.address))?.address;
const IDENTITY = globalV6 || "2a01:4f9:c013:9b52::de:1";
if (!globalV6) console.log(`[dev-run] no global IPv6 on this box — identity is a placeholder (${IDENTITY}); on a real enclave [1]==[2]==[3]`);

// --- the enclave side: SOCKS front + relay ingress (real code) ---------------
const egress = createEgress({
  secret: SECRET, socksPort: 0, relayToken: TOKEN,
  sourceAddrFor: () => IDENTITY, isKnown: (id) => id === ID,
  log: (m) => console.log("[front]", m),
});
const enclave = http.createServer((_q, res) => { res.statusCode = 404; res.end(); });
enclave.on("upgrade", (req, s, head) => { if (!egress.handleUpgrade(req, s, head)) s.destroy(); });
enclave.listen(0, "127.0.0.1"); await once(enclave, "listening");
await egress.start();
const FRONT = `127.0.0.1:${egress.socksPort()}`;

// --- the relay box side: the real fleet-aware egress relay -------------------
const relay = spawn(process.execPath, [path.join(REPO, "relay", "egress-relay.js")], {
  env: { ...process.env, ENCLAVES: `http://127.0.0.1:${enclave.address().port}`,
         EGRESS_RELAY_TOKEN: TOKEN, EGRESS_ALLOW_V4: "1" },
  stdio: ["ignore", "inherit", "inherit"],
});
for (let i = 0; i < 40 && !egress.connected(); i++) await sleep(250);
if (!egress.connected()) die("relay control channel never attached");

// --- the tenant: unmodified component under the phase-2 lockdown -------------
const cred = `${ID}:${egressToken(SECRET, ID)}`;
const guest = spawn(WASMTIME, [
  "run", "-Scli", "-Stcp", "-Sudp", "-Sallow-ip-name-lookup",
  "-S", `egress=${FRONT}`,                                   // phase 2: mediate all outbound
  "--env", `ENCLAVE_PORTS=http:8000=${HTTP_ACTUAL},tcp:7777=${TCP_ACTUAL}`,
  "--env", `ENCLAVE_EGRESS=socks5h://${cred}@${FRONT}`,          // phase 1: explicit opt-in
  "--env", "ENCLAVE_CONFIG=ipv4.icanhazip.com:80",               // v4-only echo for v4-only dev boxes
  WASM,
], { env: { ...process.env, ENCLAVE_EGRESS_CRED: cred }, stdio: ["ignore", "inherit", "inherit"] });
guest.on("close", (c) => { if (!done) die(`wasmtime exited early (${c}) — is ${WASMTIME} a phase-2 binary with -S egress?`); });

let done = false;
await waitPort(HTTP_ACTUAL);
for (const [label, port] of [["http:8000 (the /x/<id> page)", HTTP_ACTUAL], ["tcp:7777 (the dedicated-IP page)", TCP_ACTUAL]]) {
  const page = await fetchPage(port);
  console.log(`\n===== via ${label} → 127.0.0.1:${port} =====\n`);
  console.log(page);
}
done = true;
guest.kill(); relay.kill(); egress.stop(); enclave.close();
process.exit(0);

// --- helpers ------------------------------------------------------------------
function sleep(ms) { return new Promise((r) => setTimeout(r, ms)); }
function die(msg) { console.error("[dev-run] FATAL:", msg); try { relay.kill(); } catch {} process.exit(1); }
async function waitPort(port) {
  for (let i = 0; i < 60; i++) {
    const ok = await new Promise((res) => {
      const c = net.connect(port, "127.0.0.1");
      c.on("connect", () => { c.destroy(); res(true); });
      c.on("error", () => res(false));
    });
    if (ok) return;
    await sleep(250);
  }
  die(`guest never bound 127.0.0.1:${port}`);
}
function fetchPage(port) {
  return new Promise((resolve) => {
    const c = net.connect(port, "127.0.0.1");
    let buf = "";
    c.on("connect", () => c.write("GET / HTTP/1.1\r\nHost: demo\r\nConnection: close\r\n\r\n"));
    c.on("data", (d) => (buf += d));
    c.on("close", () => resolve(buf.split("\r\n\r\n").slice(1).join("\r\n\r\n") || buf));
    c.on("error", () => resolve("(fetch failed)"));
    setTimeout(() => c.destroy(), 45000);
  });
}
