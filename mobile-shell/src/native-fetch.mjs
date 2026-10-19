// A fetch over Capacitor's native HTTP bridge, wrapped back into a real Response: the verifier reads bodies as text, as
// JSON and as a byte stream (AMD collateral), and checks status. Android's bridge (HttpRequestHandler.readData) answers
// an error body as its text (or parsed, if JSON), a JSON body parsed whatever responseType asks, and any other body,
// text included, as base64 under "arraybuffer". An AbortSignal rejects the call (the bridge itself cannot be cancelled).
function b64Bytes(b64) {
  const bin = atob(String(b64).replace(/\s+/g, ""));
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function makeNativeFetch(http) {
  return async function nativeFetch(url, opts = {}) {
    const headers = { ...(opts.headers || {}) };
    const isJson = Object.entries(headers).some(([k, v]) => k.toLowerCase() === "content-type" && /json/i.test(v));
    const req = http.request({
      url: String(url),
      method: opts.method || "GET",
      headers,
      data: opts.body == null ? undefined : isJson ? JSON.parse(opts.body) : opts.body,
      responseType: "arraybuffer",
      connectTimeout: 20000,
      readTimeout: 60000,
    });
    const aborted = new Promise((_, reject) => {
      const sig = opts.signal;
      if (!sig) return;
      const no = () => reject(Object.assign(new Error("aborted"), { name: "AbortError" }));
      if (sig.aborted) no(); else sig.addEventListener("abort", no, { once: true });
    });
    const r = await Promise.race([req, aborted]);
    const ct = Object.entries(r.headers || {}).find(([k]) => k.toLowerCase() === "content-type")?.[1] || "";
    const body = typeof r.data !== "string" ? new TextEncoder().encode(JSON.stringify(r.data ?? null))
      : r.error || /json/i.test(ct) ? new TextEncoder().encode(r.data)
      : b64Bytes(r.data);
    const status = r.status >= 200 && r.status <= 599 ? r.status : 502;
    return new Response(status === 204 || status === 304 ? null : body, { status, headers: ct ? { "content-type": ct } : {} });
  };
}
