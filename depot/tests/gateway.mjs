// A stand-in for the platform's app gateway (supervisor.js `/x/:id`): Node's
// http server and client, request and response piped, hop-by-hop headers
// dropped, Authorization removed, a 180 s tenant idle timeout. Bodies are
// re-framed by Node exactly as in production (a chunked upload arrives
// chunked; a chunked response is re-chunked).
//   node tests/gateway.mjs <listen-port> <tenant-port> [keep-auth]
// (keep-auth forwards Authorization, so a whole suite that authenticates
// with URL credentials can run through the gateway's framing)
import http from "node:http";

const [listen, tenant] = process.argv.slice(2, 4).map(Number);
const keepAuth = process.argv[4] === "keep-auth";
const HOP = ["connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer",
  "transfer-encoding", "upgrade"];
const agent = new http.Agent({ keepAlive: true });
const server = http.createServer((req, res) => {
  const headers = { ...req.headers, host: `127.0.0.1:${tenant}` };
  if (!keepAuth) delete headers.authorization; // the Enclave token stays at the supervisor
  for (const h of HOP) delete headers[h];
  const up = http.request({ host: "127.0.0.1", port: tenant, method: req.method, path: req.url, headers, agent }, (r) => {
    const out = { ...r.headers };
    for (const h of HOP) delete out[h];
    res.writeHead(r.statusCode || 502, out);
    r.pipe(res);
    r.on("error", () => res.destroy());
    r.on("close", () => { if (!r.complete && !res.destroyed) res.destroy(); });
  });
  up.setTimeout(180000, () => { res.headersSent ? res.destroy() : (res.writeHead(504), res.end()); up.destroy(); });
  up.on("error", (e) => { if (res.headersSent) return res.destroy(); res.writeHead(502); res.end("upstream error: " + e.message); });
  res.on("close", () => { if (!res.writableEnded) up.destroy(); });
  req.pipe(up);
});
server.keepAliveTimeout = 180000;
server.headersTimeout = 185000;
server.requestTimeout = 0;
server.listen(listen, "127.0.0.1", () => console.log(`gateway :${listen} -> :${tenant}`));
