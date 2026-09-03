//! api-mcp-adapter: HTTP APIs described in the app config, served as ONE MCP
//! server from an attested enclave.
//!
//! The shape is s3-ipfs-adapter's applied to tools: that app takes a storage
//! API and speaks a protocol clients already know (the IPFS gateway); this
//! one takes any HTTP API and speaks the protocol agents already know (the
//! Model Context Protocol, streamable-HTTP transport). The config lists
//! endpoints in the exact form an eyesoff-ai `tools.http` block does - name,
//! description, JSON-Schema parameters, a URL with `{arg}` placeholders, a
//! body template with `$arg` holes, headers that reference `$SECRET`s - and
//! every `tools/call` is one templated request this deployment makes with its
//! own credentials. An eyesoff-ai deployment then carries one `mcp` entry
//! instead of the whole block; Claude Code, Cursor and any other MCP client
//! reach the same tools with the same key.
//!
//! WHAT THE ENCLAVE ADDS: the API keys exist only inside the attested guest,
//! referenced as `$VAR` deployment secrets; the code holding them is the code
//! the catalog pins, and that code can only make the requests the config
//! describes. A caller chooses ARGUMENTS, never a URL or a header: the
//! adapter is not an open fetcher, it is the config's endpoints and nothing
//! else. The endpoint sees the arguments and this deployment's egress IP.
//!
//! IDENTITY: an entry whose header is the reserved whole-value `$user` acts
//! for a named account and is never reached nameless. Two ways to name one:
//!   - `X-Sso-Token: EST1…`, the platform's sign-in token, verified here
//!     (src/sso.rs) against the pinned signer and this deployment's id or
//!     the ids in `sso.accept`;
//!   - the API key plus `X-User: <sub>`: the SERVICE asserting the identity,
//!     which is how eyesoff-ai names the signed-in user (its `"x-user":
//!     "$user"` header slot, filled from the token it verified, never from
//!     the model).
//! Without a name, per-user tools are absent from `tools/list` and refused
//! by `tools/call`, so a model is never shown a tool it cannot use.
//!
//! PROTOCOL: stateless streamable HTTP. `initialize` is answered but not
//! required - a client that skips the handshake (eyesoff-ai's `handshake:
//! false`, one round trip instead of three per turn) is served the same;
//! notifications get 202; no session id is minted; GET is 405 for event
//! streams and an info page for people; tools/call failures are in-band
//! (`isError`), JSON-RPC errors are reserved for protocol faults.
//!
//! Routes:
//!   GET  /              this page (status, tools, connect snippets, a probe)
//!   GET  /ping          liveness
//!   POST /mcp           the MCP endpoint (POST / works too)
//!   GET  /api/status    configured? locked? how many tools? who am I?
//!   GET  /api/tools     what tools/list would show THIS caller, the groups,
//!                       and a ready-to-paste eyesoff-ai `tools.mcp` entry
//!   GET  /api/tools?call=<name>&args=<json>   run ONE tool: separates "can
//!                       the adapter see it" from "does the endpoint work"
//! `/mcp` and `/api/tools` are behind the `api_key` when one is configured
//! (as `X-Api-Key`: the platform's app gateway consumes `Authorization`).
#[allow(warnings)]
mod bindings;
mod engine;
mod http;
mod sso;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::io::streams::StreamError;

static INDEX_HTML: &str = include_str!("index.html");
const APP: &str = "api-mcp-adapter";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The `_meta` key under which a tool carries what eyesoff-ai needs beyond
/// its schema (see engine::describe). Prefixed per the protocol's rule for
/// vendor keys, so no other server's metadata can collide with it.
pub const META_KEY: &str = "enclave.host/tool";
/// newest first; `initialize` echoes the client's when it is one of these
const PROTOCOL_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
/// a tools/call may carry the caller's attached pictures as data URIs
const MAX_BODY: usize = 32 * 1024 * 1024;
/// the most a deployment may set `max_bytes` to. A response is buffered whole
/// in guest memory before it is looked at, so this is a memory bound, not a
/// preference; the image default (12 MB) and the upscale example (64 MB) sit
/// under it.
const MAX_BYTES_CEILING: u64 = 128 * 1024 * 1024;

// ---- config ----------------------------------------------------------------

struct Config {
    title: String,
    instructions: Option<String>,
    /// the key, resolved from its `$SECRET` reference
    api_key: Option<String>,
    /// an api_key was configured but its secret is not set: refuse
    /// everything rather than silently serving open
    locked: Option<String>,
    sso: Option<sso::SsoConfig>,
    /// the config is not usable at all (not JSON, a malformed sso block)
    error: Option<String>,
    settings: engine::Settings,
    tools: Vec<engine::HttpTool>,
    /// entries that did not parse or were dropped, for the operator
    notes: Vec<String>,
}

/// Deployment secrets arrive as guest env. The platform also substitutes
/// stored `$NAME`s into the config text at launch, so on the fleet most
/// references are already resolved by the time this runs; the lookup here is
/// what makes a local `wasmtime serve --env` behave the same.
fn secret(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn load_config() -> Config {
    let raw = std::env::var("ENCLAVE_CONFIG")
        .or_else(|_| std::env::var("MCP_ADAPTER_CONFIG"))
        .unwrap_or_default();
    let mut cfg = Config {
        title: APP.to_string(),
        instructions: None,
        api_key: None,
        locked: None,
        sso: None,
        error: None,
        settings: engine::Settings::default(),
        tools: Vec::new(),
        notes: Vec::new(),
    };
    let v: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                cfg.error = Some(format!("config is not valid JSON: {e}"));
                return cfg;
            }
        }
    };
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).unwrap_or("").to_string();
    if !s("title").is_empty() {
        cfg.title = s("title");
    }
    cfg.instructions = Some(s("instructions")).filter(|i| !i.is_empty());
    let key = s("api_key");
    if !key.is_empty() {
        let (resolved, missing) = engine::expand(&key, &secret);
        match missing {
            Some(name) => {
                cfg.locked = Some(format!(
                    "api_key references ${name}, which is not set on this deployment; add the \
                     secret and restart. Refusing to serve open."
                ));
            }
            None => cfg.api_key = Some(resolved),
        }
    }
    match sso::SsoConfig::from_config(&v) {
        Ok(c) => cfg.sso = c,
        Err(e) => cfg.error = Some(format!("configuration error: {e}")),
    }
    // the entries: a top-level `http` array, or a whole eyesoff-ai `tools`
    // block pasted as-is (its `http` array is what counts; its budgets are
    // the client's business), or a bare array under `tools`
    let block = v.get("tools");
    let entries = v
        .get("http")
        .and_then(|h| h.as_array())
        .or_else(|| block.and_then(|t| t.get("http")).and_then(|h| h.as_array()))
        .or_else(|| block.and_then(|t| t.as_array()));
    let num = |k: &str| -> Option<u64> {
        v.get(k).and_then(|x| x.as_u64()).or_else(|| block.and_then(|t| t.get(k)).and_then(|x| x.as_u64()))
    };
    if let Some(t) = num("timeout_s") {
        cfg.settings.timeout_s = t.max(1);
    }
    if let Some(b) = num("max_bytes") {
        // Clamp in u64 space and THEN narrow. `as usize` would wrap on this
        // target (a wasm component is 32-bit), turning "8 GB" into a tiny cap
        // that truncates every response; try_from alone would turn it into
        // usize::MAX, which is an unbounded buffer wearing a clamp's clothes.
        // A response is held whole in guest memory, so the ceiling is real.
        let clamped = b.clamp(1024, MAX_BYTES_CEILING);
        cfg.settings.max_bytes = usize::try_from(clamped).unwrap_or(usize::MAX);
        if b > MAX_BYTES_CEILING {
            cfg.notes.push(format!(
                "max_bytes {b} is above this app's ceiling of {MAX_BYTES_CEILING}; using the ceiling"
            ));
        }
    }
    for (i, e) in entries.into_iter().flatten().enumerate() {
        let t: engine::HttpTool = match serde_json::from_value(e.clone()) {
            Ok(t) => t,
            Err(err) => {
                let name = e.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                cfg.notes.push(format!("entry {i} ('{name}') ignored: {err}"));
                continue;
            }
        };
        if let Err(err) = engine::check_name(&t.name) {
            cfg.notes.push(format!("entry {i} ('{}') ignored: {err}", t.name));
            continue;
        }
        if cfg.tools.iter().any(|o| o.name == t.name) {
            cfg.notes.push(format!("entry {i} ('{}') ignored: the name is already taken", t.name));
            continue;
        }
        if t.route.is_some() && t.route_binding().is_none() {
            cfg.notes.push(format!(
                "tool '{}': `route` needs `route_arg` (or exactly one required parameter) to bind to",
                t.name
            ));
        }
        cfg.tools.push(t);
    }
    cfg
}

impl Config {
    fn usable(&self) -> Result<(), (u16, String)> {
        if let Some(e) = &self.error {
            return Err((503, e.clone()));
        }
        if let Some(l) = &self.locked {
            return Err((503, l.clone()));
        }
        Ok(())
    }

    fn users(&self) -> bool {
        self.tools.iter().any(|t| t.requires_user())
    }

    /// The switch each tool sits under, by eyesoff-ai's rule, as {group: [names]}.
    fn groups(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut out: serde_json::Map<String, serde_json::Value> = serde_json::Map::new();
        for i in 0..self.tools.len() {
            let g = engine::group_of(&self.tools, i);
            let arr = out.entry(g).or_insert_with(|| serde_json::json!([]));
            arr.as_array_mut().unwrap().push(serde_json::json!(self.tools[i].name));
        }
        out
    }

    /// The tools THIS caller may see: per-user entries only for a named one.
    fn visible(&self, caller: &Caller) -> Vec<usize> {
        (0..self.tools.len())
            .filter(|&i| caller.sub.is_some() || !self.tools[i].requires_user())
            .collect()
    }

    fn describe(&self, i: usize) -> serde_json::Value {
        engine::describe(&self.tools[i], &engine::group_of(&self.tools, i))
    }

    fn instructions(&self) -> String {
        if let Some(i) = &self.instructions {
            return i.clone();
        }
        let mut s = String::from(
            "Every tool here is an HTTP API the deployer configured, called from inside an \
             attested enclave with the deployment's own credentials. The arguments you pass are \
             the only thing that reaches the endpoint. Read a failed call's message: it names a \
             missing argument, a missing secret, or the endpoint's own error.",
        );
        if self.users() {
            s.push_str(
                " Some tools act for a signed-in Enclave account and are only listed when the \
                 request names one (an X-Sso-Token, or the API key with X-User).",
            );
        }
        s
    }
}

// ---- request plumbing ------------------------------------------------------

const CORS: [(&str, &str); 4] = [
    ("access-control-allow-origin", "*"),
    ("access-control-allow-methods", "POST, GET, OPTIONS"),
    (
        "access-control-allow-headers",
        "Content-Type, Authorization, X-Api-Key, X-User, X-Sso-Token, Mcp-Session-Id, Mcp-Protocol-Version, Last-Event-ID",
    ),
    ("access-control-max-age", "600"),
];

fn respond(out: ResponseOutparam, status: u16, ctype: &str, body_bytes: &[u8], extra: &[(&str, &str)]) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-store".to_vec()]);
    for (k, v) in extra {
        let _ = headers.set(&k.to_string(), &[v.as_bytes().to_vec()]);
    }
    let resp = OutgoingResponse::new(headers);
    let _ = resp.set_status_code(status);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    // the platform caps a single body write at 4096 bytes
    for chunk in body_bytes.chunks(4000) {
        if stream.blocking_write_and_flush(chunk).is_err() {
            break;
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

fn json(out: ResponseOutparam, status: u16, v: serde_json::Value) {
    respond(out, status, "application/json", v.to_string().as_bytes(), &[]);
}

fn json_err(out: ResponseOutparam, status: u16, msg: &str) {
    json(out, status, serde_json::json!({ "error": { "message": msg } }));
}

/// An MCP-side answer: JSON with CORS, so a browser-hosted client can dial.
fn mcp_json(out: ResponseOutparam, status: u16, v: &serde_json::Value) {
    respond(out, status, "application/json", v.to_string().as_bytes(), &CORS);
}

fn read_body(req: &IncomingRequest, cap: usize) -> Result<Vec<u8>, ()> {
    let mut out = Vec::new();
    let Ok(body) = req.consume() else { return Ok(out) };
    let Ok(stream) = body.stream() else { return Ok(out) };
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if out.len() > cap {
                    return Err(());
                }
            }
            Err(StreamError::Closed) => break,
            Err(_) => break,
        }
    }
    Ok(out)
}

fn header(req: &IncomingRequest, name: &str) -> Option<String> {
    req.headers()
        .get(&name.to_string())
        .into_iter()
        .next()
        .map(|v| String::from_utf8_lossy(&v).into_owned())
}

/// `?a=b&c=d` -> pairs, percent-decoded ('+' as space, the form convention).
fn query_pairs(pq: &str) -> Vec<(String, String)> {
    let Some((_, q)) = pq.split_once('?') else { return Vec::new() };
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (
                pct_decode(&k.replace('+', " ")).unwrap_or_default(),
                pct_decode(&v.replace('+', " ")).unwrap_or_default(),
            )
        })
        .collect()
}

fn query_get<'q>(q: &'q [(String, String)], k: &str) -> Option<&'q str> {
    q.iter().find(|(key, _)| key == k).map(|(_, v)| v.as_str())
}

fn pct_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let h = s.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(h, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// constant-time equality for the API key
fn key_matches(want: &str, got: &str) -> bool {
    let (a, b) = (want.as_bytes(), got.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn keyed(cfg: &Config, req: &IncomingRequest) -> bool {
    let Some(want) = cfg.api_key.as_deref() else { return false };
    if let Some(auth) = header(req, "authorization") {
        if let Some(tok) = auth.strip_prefix("Bearer ").or_else(|| auth.strip_prefix("bearer ")) {
            if key_matches(want, tok.trim()) {
                return true;
            }
        }
    }
    if let Some(k) = header(req, "x-api-key") {
        if key_matches(want, k.trim()) {
            return true;
        }
    }
    false
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The sign-in token a request carries, if any: `X-Sso-Token` (the header
/// that survives the platform gateway), or an EST1 bearer for direct use.
fn sso_token_of(req: &IncomingRequest) -> Option<String> {
    if let Some(t) = header(req, "x-sso-token").map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        return Some(t);
    }
    header(req, "authorization")
        .and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer ")).map(|t| t.trim().to_string()))
        .filter(|t| t.starts_with("EST1."))
}

/// Who a request is, once its credentials have been checked.
struct Caller {
    /// the named account, when the request names one
    sub: Option<String>,
    /// how the name was established: "sso" (a verified token) or "key"
    /// (the service asserting it with X-User)
    via: &'static str,
    /// may this request use the tools at all: an open deployment, the API
    /// key, or a verified sign-in token from an accepted audience
    authorized: bool,
}

fn identify(cfg: &Config, req: &IncomingRequest) -> Result<Caller, (u16, String)> {
    let has_key = keyed(cfg, req);
    // A locked or unusable deployment authorizes NOBODY. Every route checks
    // usable() first, but seeding it here means a future caller that forgets
    // cannot accidentally hand out an authorized caller.
    let usable = cfg.usable().is_ok();
    // THE GATE IS THE KEY. A sign-in token NAMES the caller, it does not open
    // the door: a deployment that sets an api_key means it, and the `accept`
    // list is a statement about whose identities are meaningful here, not
    // about who may call. Without a key configured there is no door, and a
    // token is simply a name.
    let mut caller =
        Caller { sub: None, via: "", authorized: usable && (cfg.api_key.is_none() || has_key) };
    if let Some(tok) = sso_token_of(req) {
        if let Some(sso_cfg) = &cfg.sso {
            return match sso::verify(sso_cfg, &tok, now_secs()) {
                Ok(c) => Ok(Caller { sub: Some(c.sub), via: "sso", authorized: caller.authorized }),
                Err(e) => Err((401, format!("[sso_required] {e}"))),
            };
        }
        // a token this deployment cannot verify names nobody; the request
        // proceeds on whatever else it carries
    }
    // the service path: only a real key counts (an open deployment has no
    // service to trust), and the name must be one of the platform's shapes
    if has_key {
        if let Some(u) = header(req, "x-user").map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) {
            match sso::canonical_sub(&u) {
                Some(sub) => {
                    caller.sub = Some(sub);
                    caller.via = "key";
                }
                None => {
                    return Err((400, "X-User must be an Enclave identity: a 0x wallet address or an acct_ id".into()))
                }
            }
        }
    }
    Ok(caller)
}

/// The deployment's own public origin: what the client dialled, as the
/// gateway forwarded it.
fn origin_of(req: &IncomingRequest) -> String {
    let authority = req.authority().unwrap_or_else(|| "localhost".to_string());
    // an IPv6 literal is bracketed and full of colons, so splitting on ':'
    // mangles it into "[" and it stops looking like an address at all -
    // which would advertise https://[::1] to a local client
    let host = if authority.starts_with('[') {
        authority.split(']').next().unwrap_or("").trim_start_matches('[')
    } else {
        authority.split(':').next().unwrap_or("")
    };
    let local = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok();
    let https = match req.scheme() {
        Some(Scheme::Https) => true,
        Some(Scheme::Http) => !local,
        _ => !local,
    };
    format!("{}://{authority}", if https { "https" } else { "http" })
}

// ---- MCP -------------------------------------------------------------------

fn rpc_error(id: &serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": if id.is_null() { serde_json::Value::Null } else { id.clone() },
                        "error": { "code": code, "message": message } })
}

fn rpc_result(id: &serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

/// A tools/call failure, in-band: the model reads it and decides, which is
/// what the protocol wants for "the tool ran and did not like it".
fn tool_error(msg: &str) -> serde_json::Value {
    serde_json::json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

/// A tools/call success: the picture first when there is one (a client
/// that renders images shows it; eyesoff-ai delivers it to the chat), then
/// the text; citation rows under `structuredContent.sources`.
fn tool_result(o: engine::Outcome) -> serde_json::Value {
    let mut content = Vec::new();
    if let Some(img) = &o.image {
        content.push(serde_json::json!({ "type": "image", "data": img.b64, "mimeType": img.mime }));
    }
    content.push(serde_json::json!({ "type": "text", "text": o.text }));
    let mut r = serde_json::json!({ "content": content });
    if !o.sources.is_empty() {
        r["structuredContent"] = serde_json::json!({
            "sources": o.sources.iter().map(|(t, u)| serde_json::json!({ "title": t, "url": u })).collect::<Vec<_>>()
        });
    }
    r
}

fn call_tool(cfg: &Config, caller: &Caller, name: &str, args: &serde_json::Value) -> serde_json::Value {
    let Some(t) = cfg.tools.iter().find(|t| t.name == name) else {
        let known: Vec<&str> = cfg.visible(caller).into_iter().map(|i| cfg.tools[i].name.as_str()).collect();
        return tool_error(&format!(
            "there is no tool named '{name}' on this deployment. Available: {}",
            if known.is_empty() { "(none)".to_string() } else { known.join(", ") }
        ));
    };
    let args = match args {
        serde_json::Value::Null => serde_json::json!({}),
        serde_json::Value::Object(_) => args.clone(),
        _ => return tool_error("arguments must be a JSON object"),
    };
    let missing = t.missing_required(&args);
    if !missing.is_empty() {
        return tool_error(&format!(
            "missing required argument{} for {name}: {}",
            if missing.len() == 1 { "" } else { "s" },
            missing.join(", ")
        ));
    }
    match engine::call(t, cfg.settings, &args, caller.sub.as_deref(), &secret) {
        Ok(o) => tool_result(o),
        Err(e) => tool_error(&e),
    }
}

/// One JSON-RPC message in, at most one out (None for a notification or a
/// client response, which the transport answers with 202).
fn dispatch(cfg: &Config, caller: &Caller, msg: &serde_json::Value) -> Option<serde_json::Value> {
    let null = serde_json::Value::Null;
    let id = msg.get("id").unwrap_or(&null);
    if !msg.is_object() || msg.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return Some(rpc_error(id, -32600, "Invalid Request: expected a JSON-RPC 2.0 message"));
    }
    let Some(method) = msg.get("method").and_then(|m| m.as_str()) else { return None };
    if id.is_null() {
        return None; // a notification (initialized, cancelled, progress, ...)
    }
    let params = msg.get("params").unwrap_or(&null);
    Some(match method {
        "initialize" => {
            let want = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("");
            let version = if PROTOCOL_VERSIONS.contains(&want) { want } else { PROTOCOL_VERSIONS[0] };
            rpc_result(id, serde_json::json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": APP, "title": cfg.title, "version": VERSION },
                "instructions": cfg.instructions(),
            }))
        }
        "ping" => rpc_result(id, serde_json::json!({})),
        "tools/list" => rpc_result(id, serde_json::json!({
            "tools": cfg.visible(caller).into_iter().map(|i| cfg.describe(i)).collect::<Vec<_>>()
        })),
        "tools/call" => match params.get("name").and_then(|n| n.as_str()) {
            Some(name) => rpc_result(id, call_tool(cfg, caller, name, params.get("arguments").unwrap_or(&null))),
            None => rpc_error(id, -32602, "params.name is required"),
        },
        other => rpc_error(id, -32601, &format!("Method not found: {other}")),
    })
}

fn handle_mcp_post(cfg: &Config, req: IncomingRequest, out: ResponseOutparam) {
    if let Err((status, msg)) = cfg.usable() {
        return mcp_json(out, status, &rpc_error(&serde_json::Value::Null, -32000, &msg));
    }
    let caller = match identify(cfg, &req) {
        Ok(c) => c,
        Err((status, msg)) => return mcp_json(out, status, &rpc_error(&serde_json::Value::Null, -32001, &msg)),
    };
    if !caller.authorized {
        return mcp_json(
            out,
            401,
            &rpc_error(
                &serde_json::Value::Null,
                -32001,
                "unauthorized: send the deployment's API key as `X-Api-Key: <key>` (on enclave.host the gateway consumes `Authorization`, so a bearer never arrives here), or a sign-in token as `X-Sso-Token`",
            ),
        );
    }
    let Ok(body) = read_body(&req, MAX_BODY) else {
        return mcp_json(out, 413, &rpc_error(&serde_json::Value::Null, -32600, "request body too large"));
    };
    let msg: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return mcp_json(out, 400, &rpc_error(&serde_json::Value::Null, -32700, &format!("Parse error: {e}"))),
    };
    // 2025-03-26 batching compat: an array in, an array of the answers out
    if let Some(batch) = msg.as_array() {
        let answers: Vec<serde_json::Value> = batch.iter().filter_map(|m| dispatch(cfg, &caller, m)).collect();
        if answers.is_empty() {
            return respond(out, 202, "application/json", b"", &CORS);
        }
        return mcp_json(out, 200, &serde_json::Value::Array(answers));
    }
    match dispatch(cfg, &caller, &msg) {
        Some(answer) => mcp_json(out, 200, &answer),
        None => respond(out, 202, "application/json", b"", &CORS),
    }
}

fn handle_mcp_get(cfg: &Config, req: &IncomingRequest, out: ResponseOutparam) {
    // no server-initiated stream: SSE clients get the spec's 405, people
    // get directions
    if header(req, "accept").is_some_and(|a| a.contains("text/event-stream")) {
        // CORS on the refusal too: without it a browser client sees an
        // opaque network error instead of "this server has no event stream"
        let mut h: Vec<(&str, &str)> = vec![("allow", "POST, GET, DELETE, OPTIONS")];
        h.extend_from_slice(&CORS);
        return respond(out, 405, "text/plain", b"POST JSON-RPC here; this server opens no event stream", &h);
    }
    let origin = origin_of(req);
    mcp_json(out, 200, &serde_json::json!({
        "name": APP, "title": cfg.title, "version": VERSION,
        "protocol": "Model Context Protocol (streamable HTTP, stateless)",
        "endpoint": format!("{origin}/mcp"),
        "tools": cfg.tools.len(),
        "auth": if cfg.api_key.is_some() { "X-Api-Key: <key>" } else { "open" },
        "connect": {
            "claude_code": format!("claude mcp add --transport http {} {origin}/mcp{}", slug(&cfg.title),
                                   if cfg.api_key.is_some() { " --header \"X-Api-Key: <key>\"" } else { "" }),
            "generic": "POST initialize / tools/list / tools/call as JSON-RPC 2.0",
        },
        "ui": origin,
    }));
}

/// A name a CLI can take: letters, digits and dashes.
fn slug(title: &str) -> String {
    let mut s = String::new();
    let mut dash = false;
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
            dash = false;
        } else if !dash && !s.is_empty() {
            s.push('-');
            dash = true;
        }
    }
    let s = s.trim_end_matches('-').to_string();
    if s.is_empty() { APP.to_string() } else { s }
}

// ---- /api ------------------------------------------------------------------

fn handle_status(cfg: &Config, req: &IncomingRequest, out: ResponseOutparam) {
    let mut v = serde_json::json!({
        "app": APP, "version": VERSION, "title": cfg.title,
        "configured": cfg.usable().is_ok() && !cfg.tools.is_empty(),
        "auth": cfg.api_key.is_some(),
        "users": cfg.users(),
        "sso": cfg.sso.is_some(),
        "tools": cfg.tools.len(),
        "groups": cfg.groups().keys().cloned().collect::<Vec<_>>(),
        "notes": cfg.notes,
        "timeout_s": cfg.settings.timeout_s,
        "max_bytes": cfg.settings.max_bytes,
    });
    if let Err((_, e)) = cfg.usable() {
        v["error"] = serde_json::json!(e);
    }
    if let Some(sc) = &cfg.sso {
        v["sso_config"] = serde_json::json!({ "authorize_url": sc.authorize_url, "aud": sc.audience, "accept": sc.accept.len() });
    }
    if let Ok(c) = identify(cfg, req) {
        // a locked deployment refuses every route, so it must not tell a
        // caller they are authorized: cfg.api_key is None while locked, and
        // identify() reads that as "open"
        v["authorized"] = serde_json::json!(c.authorized);
        if let Some(sub) = c.sub {
            v["you"] = serde_json::json!({ "sub": sub, "via": c.via });
        }
    }
    json(out, 200, v);
}

fn handle_tools(cfg: &Config, req: &IncomingRequest, q: &[(String, String)], out: ResponseOutparam) {
    if let Err((status, msg)) = cfg.usable() {
        return json_err(out, status, &msg);
    }
    let caller = match identify(cfg, req) {
        Ok(c) => c,
        Err((status, msg)) => return json_err(out, status, &msg),
    };
    if !caller.authorized {
        return json_err(out, 401, "missing or invalid API key: send it as `X-Api-Key: <key>`");
    }
    if let Some(name) = query_get(q, "call") {
        let args: serde_json::Value = match query_get(q, "args") {
            Some(a) if !a.trim().is_empty() => match serde_json::from_str(a) {
                Ok(v) => v,
                Err(e) => return json_err(out, 400, &format!("args is not valid JSON: {e}")),
            },
            _ => serde_json::json!({}),
        };
        let t0 = now_ms();
        let r = call_tool(cfg, &caller, name, &args);
        let ms = now_ms().saturating_sub(t0);
        let is_error = r.get("isError").and_then(|e| e.as_bool()).unwrap_or(false);
        let mut body = serde_json::json!({ "name": name, "arguments": args, "ok": !is_error, "ms": ms });
        let mut texts = Vec::new();
        if let Some(parts) = r.get("content").and_then(|c| c.as_array()) {
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    Some("text") => texts.push(p.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string()),
                    Some("image") => {
                        let (data, mime) = (
                            p.get("data").and_then(|d| d.as_str()).unwrap_or(""),
                            p.get("mimeType").and_then(|m| m.as_str()).unwrap_or("image/png"),
                        );
                        body["image"] = serde_json::json!({
                            "mime": mime, "base64_bytes": data.len(),
                            "data_uri": format!("data:{mime};base64,{data}"),
                        });
                    }
                    _ => {}
                }
            }
        }
        body["result"] = serde_json::json!(texts.join("\n"));
        if let Some(src) = r.get("structuredContent").and_then(|s| s.get("sources")) {
            body["sources"] = src.clone();
        }
        if let Some(sub) = &caller.sub {
            body["user"] = serde_json::json!(sub);
        }
        return json(out, 200, body);
    }
    let origin = origin_of(req);
    let mcp = format!("{origin}/mcp");
    let visible = cfg.visible(&caller);
    let hidden: Vec<&str> = (0..cfg.tools.len())
        .filter(|i| !visible.contains(i))
        .map(|i| cfg.tools[i].name.as_str())
        .collect();
    let mut headers = serde_json::Map::new();
    if cfg.api_key.is_some() {
        headers.insert("x-api-key".into(), serde_json::json!("$MCP_ADAPTER_API_KEY"));
    }
    if cfg.users() {
        // eyesoff-ai fills $user from the sign-in it verified for the turn,
        // never from the model; without a signed-in caller it sends nothing
        // and this server lists no per-user tool
        headers.insert("x-user".into(), serde_json::json!("$user"));
    }
    let entry = serde_json::json!({
        "url": mcp,
        "handshake": false,
        "headers": headers,
        "groups": cfg.groups(),
    });
    json(out, 200, serde_json::json!({
        "base_url": origin,
        "mcp": mcp,
        "auth": if cfg.api_key.is_some() {
            "X-Api-Key: <api_key> (Authorization: Bearer works only off-platform: the enclave.host gateway consumes that header); per-user tools also need X-User: <sub> beside the key, or X-Sso-Token: <Enclave sign-in token>"
        } else { "open: no api_key configured" },
        "users": cfg.users(),
        "you": caller.sub,
        "tools": visible.iter().map(|&i| cfg.describe(i)).collect::<Vec<_>>(),
        "hidden": hidden,
        "groups": cfg.groups(),
        "notes": cfg.notes,
        "eyesoff_ai": { "tools": { "mcp": [entry] } },
        "claude_code": format!("claude mcp add --transport http {} {mcp}{}", slug(&cfg.title),
                               if cfg.api_key.is_some() { " --header \"X-Api-Key: <key>\"" } else { "" }),
        "curl": format!("curl -s {mcp} -H 'content-type: application/json'{} -d '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}}'",
                        if cfg.api_key.is_some() { " -H 'x-api-key: <key>'" } else { "" }),
    }));
}

// ---- routing ---------------------------------------------------------------

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/").trim_end_matches('/').to_string();
        let path = if path.is_empty() { "/".to_string() } else { path };
        let method = req.method();

        if matches!(method, Method::Options) {
            return respond(out, 204, "text/plain", b"", &CORS);
        }
        match (&method, path.as_str()) {
            (Method::Get, "/") => {
                return respond(out, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes(), &[]);
            }
            (Method::Get, "/ping") => return json(out, 200, serde_json::json!({ "ok": true, "pong": true })),
            _ => {}
        }

        let cfg = load_config();
        let q = query_pairs(&pq);
        match (&method, path.as_str()) {
            (Method::Post, "/mcp") | (Method::Post, "/") => handle_mcp_post(&cfg, req, out),
            (Method::Get, "/mcp") => handle_mcp_get(&cfg, &req, out),
            (Method::Delete, "/mcp") => {
                let mut h: Vec<(&str, &str)> = vec![("allow", "POST, GET, DELETE, OPTIONS")];
                h.extend_from_slice(&CORS);
                respond(out, 405, "text/plain", b"stateless: there is no session to end", &h)
            }
            (Method::Get, "/api/status") => handle_status(&cfg, &req, out),
            (Method::Get, "/api/tools") => handle_tools(&cfg, &req, &q, out),
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /ping, POST /mcp, GET /mcp, GET /api/status, GET /api/tools[?call=<name>&args=<json>]",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(tools: serde_json::Value) -> Config {
        let mut c = Config {
            title: "test tools".into(),
            instructions: None,
            api_key: Some("k".into()),
            locked: None,
            sso: None,
            error: None,
            settings: engine::Settings::default(),
            tools: serde_json::from_value(tools).unwrap(),
            notes: Vec::new(),
        };
        c.title = "test tools".into();
        c
    }

    fn anon() -> Caller {
        Caller { sub: None, via: "", authorized: true }
    }

    fn named() -> Caller {
        Caller { sub: Some("0xabc".into()), via: "key", authorized: true }
    }

    /// The protocol surface: initialize echoes a supported version and falls
    /// back to the newest, ping answers, notifications are silent, unknown
    /// methods are -32601, and a malformed message is -32600.
    #[test]
    fn json_rpc_dispatch() {
        let c = cfg(serde_json::json!([{ "name": "a", "url": "https://h" }]));
        let init = dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-03-26" } })).unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], APP);
        assert!(init["result"]["instructions"].as_str().unwrap().contains("attested enclave"));
        let init = dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 2, "method": "initialize", "params": { "protocolVersion": "1999-01-01" } })).unwrap();
        assert_eq!(init["result"]["protocolVersion"], PROTOCOL_VERSIONS[0]);
        assert_eq!(dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 3, "method": "ping" })).unwrap()["result"], serde_json::json!({}));
        assert!(dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
        assert!(dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 9, "result": {} })).is_none());
        let e = dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 4, "method": "resources/list" })).unwrap();
        assert_eq!(e["error"]["code"], -32601);
        let e = dispatch(&c, &anon(), &serde_json::json!({ "id": 5, "method": "ping" })).unwrap();
        assert_eq!(e["error"]["code"], -32600);
        let e = dispatch(&c, &anon(), &serde_json::json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/call", "params": {} })).unwrap();
        assert_eq!(e["error"]["code"], -32602);
    }

    /// Per-user tools exist only for a named caller: absent from the list
    /// and refused in-band otherwise; the rest are listed for everyone.
    #[test]
    fn per_user_tools_follow_the_name() {
        let c = cfg(serde_json::json!([
            { "name": "notes_read", "url": "https://h/{name}", "headers": { "x-user": "$user" } },
            { "name": "weather", "url": "https://h/w" }
        ]));
        assert!(c.users());
        let list = |caller: &Caller| -> Vec<String> {
            let r = dispatch(&c, caller, &serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })).unwrap();
            r["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        assert_eq!(list(&anon()), vec!["weather"]);
        assert_eq!(list(&named()), vec!["notes_read", "weather"]);
        let r = call_tool(&c, &anon(), "notes_read", &serde_json::json!({ "name": "x" }));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("signed-in user"));
        // unknown names list what IS callable for this caller
        let r = call_tool(&c, &anon(), "nope", &serde_json::json!({}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("Available: weather"));
        // required arguments are checked before anything is sent
        let c = cfg(serde_json::json!([{ "name": "s", "url": "https://h", "parameters": { "type": "object", "required": ["q"] } }]));
        let r = call_tool(&c, &anon(), "s", &serde_json::json!({}));
        assert!(r["content"][0]["text"].as_str().unwrap().contains("missing required argument for s: q"));
        let r = call_tool(&c, &anon(), "s", &serde_json::json!("not an object"));
        assert!(r["content"][0]["text"].as_str().unwrap().contains("JSON object"));
    }

    /// The result shapes: a picture leads the content, text follows, and
    /// sources ride structuredContent.
    #[test]
    fn results_are_mcp_content() {
        let r = tool_result(engine::Outcome {
            text: "hello".into(),
            sources: vec![("T".into(), "https://a".into())],
            image: None,
        });
        assert_eq!(r["content"], serde_json::json!([{ "type": "text", "text": "hello" }]));
        assert_eq!(r["structuredContent"]["sources"][0]["url"], "https://a");
        let r = tool_result(engine::Outcome {
            text: "made".into(),
            sources: vec![],
            image: Some(engine::Picture { mime: "image/webp".into(), b64: "AA==".into() }),
        });
        assert_eq!(r["content"][0], serde_json::json!({ "type": "image", "data": "AA==", "mimeType": "image/webp" }));
        assert_eq!(r["content"][1]["text"], "made");
        assert!(r.get("structuredContent").is_none());
        assert_eq!(tool_error("bad")["isError"], true);
    }

    #[test]
    fn groups_and_slugs() {
        let c = cfg(serde_json::json!([
            { "name": "notes_read", "url": "https://h" }, { "name": "notes_write", "url": "https://h" },
            { "name": "generate_image", "url": "https://h", "result": { "image": "d" } }
        ]));
        let g = c.groups();
        assert_eq!(g["notes"], serde_json::json!(["notes_read", "notes_write"]));
        assert_eq!(g["images"], serde_json::json!(["generate_image"]));
        assert_eq!(slug("Steven's eyesoff tools"), "steven-s-eyesoff-tools");
        assert_eq!(slug("!!!"), APP);
    }

    #[test]
    fn query_parsing_decodes() {
        let q = query_pairs("/api/tools?call=notes_read&args=%7B%22name%22%3A%22a%20b%22%7D");
        assert_eq!(query_get(&q, "call"), Some("notes_read"));
        assert_eq!(query_get(&q, "args"), Some(r#"{"name":"a b"}"#));
        assert!(query_pairs("/api/tools").is_empty());
        assert!(key_matches("abc", "abc") && !key_matches("abc", "abd") && !key_matches("abc", "ab"));
    }
}
