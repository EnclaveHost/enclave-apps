//! jot: an agent's notebook, kept as plain objects in the deployer's own
//! S3-compatible bucket, served as a bearer-keyed JSON API from an attested
//! enclave.
//!
//! The shape is risc-box's storage, not keep's: no encrypted volume, no
//! wallet unlock, no browser in the loop. The App Config names the bucket and
//! references the S3 credentials and the API key as `$VAR` deployment secrets;
//! every note operation is one SigV4-signed request the app makes itself over
//! wasi:http. That is what makes it usable by a program: an agent with the
//! key can read and write notes with nothing to unlock and nothing that dies
//! on a restart, because the durable copy is the object in the bucket.
//!
//! What the enclave adds: the credentials and the key exist only inside the
//! attested guest (secrets are injected as guest env by the enclave holding
//! the lease), and the code that holds them is the code the on-chain catalog
//! pins. The bucket operator sees objects and this deployment's egress IP;
//! the host operator sees ciphertext on the wire. Notes are stored as plain
//! objects on purpose, so the bucket stays readable with ordinary S3 tools;
//! a notebook that must be unreadable at rest belongs on an encrypted volume
//! (see keep).
//!
//! PER-USER MODE (an `sso` block in the config) makes this one notebook per
//! signed-in Enclave account: every request must name a user, and the notes
//! it can reach live under `<prefix>users/<sub>/`. Two ways to name one:
//!   - `X-Sso-Token: EST1…`, the platform's sign-in token, verified here
//!     (src/sso.rs) against the pinned signer and this deployment's id OR the
//!     ids in `sso.accept`: the eyesoff-ai instances this notebook serves,
//!     which forward the token they verified;
//!   - the deployment's API key plus `X-User: <sub>`: the SERVICE asserting
//!     the identity. Only an eyesoff-ai holding the key can do this, and it
//!     fills the header from the token it verified, never from anything the
//!     model wrote (tool headers come from that deployment's config).
//! Without the key, X-User is ignored; without an sso block, X-User and
//! X-Sso-Token are ignored and the notebook is one shared space.
//!
//! ENCRYPTION AT REST (a `master_key` in the config, as a `$VAR` secret): every
//! note is sealed with AES-256-GCM under a key derived from the master secret
//! and its owner (src/crypt.rs). The bucket sees names and ciphertext.
//!
//! Routes (JSON unless noted; `/api/*` except status and tools need the key
//! when one is configured, as `X-Api-Key: <key>`. `Authorization: Bearer` is
//! accepted too, but the platform's app gateway strips that header on the way
//! in (it is the carriage for the owner's own session token), so on
//! enclave.host only X-Api-Key ever arrives):
//!   GET    /                          the notebook UI (self-contained HTML)
//!   GET    /sso-return                the sign-in popup's landing pad (per-user mode)
//!   GET    /ping                      liveness
//!   GET    /api/status                configured? auth? read-only? (+bucket facts with the key)
//!   GET    /api/tools                 tool schemas for agents (OpenAI functions + an eyesoff-ai block)
//!   GET    /api/notes?prefix=&limit=  list
//!   GET    /api/notes/<name>          read: {name, content, etag, size, modified}; ?raw=1 = bytes
//!   PUT    /api/notes/<name>          write: {content, ifMatch?} or a text/* body (POST works too)
//!   POST   /api/notes/<name>/append   {content}: append a paragraph (conditional on the read ETag)
//!   DELETE /api/notes/<name>          delete (POST /api/notes/<name>/delete works too)
//!   GET    /api/search?q=&prefix=&limit=  case-insensitive substring search over note bodies
#[allow(warnings)]
mod bindings;
mod crypt;
mod http;
mod s3;
mod sso;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam, Scheme,
};
use bindings::wasi::io::streams::StreamError;

static INDEX_HTML: &str = include_str!("index.html");
static SSO_RETURN_HTML: &str = include_str!("sso-return.html");
const APP: &str = "jot";
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// a note's content caps at 1 MiB
const MAX_NOTE: usize = 1024 * 1024;
/// a request body: a 1 MiB note escaped into JSON can double
const MAX_BODY: usize = 2 * MAX_NOTE + 64 * 1024;
const MAX_NAME: usize = 200;
const LIST_DEFAULT: usize = 200;
const LIST_MAX: usize = 1000;
const SEARCH_SCAN: usize = 200;
const SEARCH_SKIP_OVER: u64 = 256 * 1024;
const SEARCH_DEFAULT: usize = 20;
const SEARCH_MAX: usize = 100;

// ---- config ----------------------------------------------------------------

struct Config {
    title: String,
    endpoint: String,
    region: String,
    bucket: String,
    prefix: String,
    creds: Option<s3::Creds>,
    api_key: Option<String>,
    read_only: bool,
    /// per-user mode: who may use the notebook and how they prove it
    sso: Option<sso::SsoConfig>,
    /// encryption at rest, when set
    master_key: Option<String>,
    /// required fields still empty, typically `$VAR` secrets not set yet
    missing: Vec<&'static str>,
    /// a config block that parsed but is wrong (a malformed sso block):
    /// the app serves and says so, but refuses every note route
    error: Option<String>,
}

/// Resolve config string values of the exact form `$NAME` / `${NAME}` from
/// the process environment, which is where deployment secrets arrive. Whole
/// value references only. An unresolved reference becomes "" (logged), which
/// downstream treats as absent: the app still serves, it just reports the gap.
fn expand_env_refs(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => {
            let Some(reference) = s.strip_prefix('$') else { return };
            let name = reference.strip_prefix('{').and_then(|r| r.strip_suffix('}')).unwrap_or(reference);
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return; // not a $NAME reference; leave the literal alone
            }
            match std::env::var(name) {
                Ok(val) => *s = val,
                Err(_) => {
                    eprintln!("[jot] config: ${name} is not set in the environment; treating the value as absent");
                    s.clear();
                }
            }
        }
        serde_json::Value::Array(a) => a.iter_mut().for_each(expand_env_refs),
        serde_json::Value::Object(o) => o.values_mut().for_each(expand_env_refs),
        _ => {}
    }
}

fn load_config() -> Config {
    let raw = std::env::var("ENCLAVE_CONFIG").or_else(|_| std::env::var("JOT_CONFIG")).unwrap_or_default();
    let mut v: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[jot] config is not valid JSON ({e}); running unconfigured");
                serde_json::json!({})
            }
        }
    };
    expand_env_refs(&mut v);
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::trim).unwrap_or("").to_string();
    let creds = v.get("credentials").and_then(|c| {
        let ak = c.get("accessKeyId")?.as_str()?.trim();
        let sk = c.get("secretAccessKey")?.as_str()?.trim();
        if ak.is_empty() || sk.is_empty() {
            return None;
        }
        Some(s3::Creds {
            access_key_id: ak.to_string(),
            secret_access_key: sk.to_string(),
            session_token: c
                .get("sessionToken")
                .and_then(|x| x.as_str())
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string),
        })
    });
    let mut prefix = s("prefix").trim_start_matches('/').to_string();
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
    let region = { let r = s("region"); if r.is_empty() { "auto".to_string() } else { r } };
    let endpoint = s("endpoint");
    let bucket = s("bucket");
    let mut missing = Vec::new();
    if endpoint.is_empty() {
        missing.push("endpoint");
    }
    if bucket.is_empty() {
        missing.push("bucket");
    }
    let (sso, error) = match sso::SsoConfig::from_config(&v) {
        Ok(c) => (c, None),
        Err(e) => (None, Some(format!("configuration error: {e}"))),
    };
    let master_key = Some(s("master_key")).filter(|k| !k.is_empty());
    if let Some(k) = &master_key {
        if k.len() < 16 {
            eprintln!("[jot] config: master_key is short ({} chars); use 32+ random characters", k.len());
        }
    }
    Config {
        title: { let t = s("title"); if t.is_empty() { "jot".to_string() } else { t } },
        endpoint,
        region,
        bucket,
        prefix,
        creds,
        api_key: Some(s("api_key")).filter(|k| !k.is_empty()),
        read_only: v.get("readOnly").and_then(|x| x.as_bool()).unwrap_or(false),
        sso,
        master_key,
        missing,
        error,
    }
}

// ---- request plumbing ------------------------------------------------------

fn respond(out: ResponseOutparam, status: u16, ctype: &str, body_bytes: &[u8]) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-store".to_vec()]);
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
    respond(out, status, "application/json", v.to_string().as_bytes());
}

fn json_err(out: ResponseOutparam, status: u16, msg: &str) {
    json(out, status, serde_json::json!({ "error": { "message": msg } }));
}

/// The S3 client's failure, as the API's answer: the store's own status
/// where it is meaningful to the caller (404, 412), 502 for the rest.
fn s3_err(out: ResponseOutparam, e: s3::S3Error) {
    let status = match &e {
        s3::S3Error::Http(412, _) => 412,
        s3::S3Error::Http(404, _) => 404,
        s3::S3Error::Http(401 | 403, _) => 502,
        s3::S3Error::Http(_, _) => 502,
        s3::S3Error::Transport(_) => 502,
    };
    let mut msg = e.message();
    if let s3::S3Error::Http(401 | 403, _) = &e {
        msg.push_str(" (the bucket refused this deployment's credentials: check the S3 secrets)");
    }
    eprintln!("[jot] s3: {msg}");
    json_err(out, status, &msg);
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

/// A note name is a relative path of plain segments: letters, digits,
/// `- _ . space`, joined by single slashes. No `.`/`..` segments, no leading
/// or trailing slash, at most MAX_NAME bytes. That is what keeps a name a key
/// UNDER the configured prefix and nothing else.
fn valid_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME || name.starts_with('/') || name.ends_with('/') {
        return false;
    }
    name.split('/').all(|seg| {
        !seg.is_empty()
            && seg != "."
            && seg != ".."
            && seg.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ' '))
    })
}

/// constant-time equality for the API key
fn key_matches(want: &str, got: &str) -> bool {
    let (a, b) = (want.as_bytes(), got.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn authorized(cfg: &Config, req: &IncomingRequest) -> bool {
    let Some(want) = cfg.api_key.as_deref() else { return true };
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

/// Who a request acts as, once its credentials have been checked.
enum Caller {
    /// the API key, in a shared (non per-user) notebook
    Service,
    /// a named account; `via` says how the name was established
    User { sub: String, via: &'static str },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
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

/// Establish the caller. Per-user mode needs a NAME, from a verified sign-in
/// token or from the service (API key + X-User); a shared notebook needs the
/// key alone. Errors are (status, message).
fn identify(cfg: &Config, req: &IncomingRequest) -> Result<Caller, (u16, String)> {
    let has_key = authorized(cfg, req);
    let Some(sso_cfg) = &cfg.sso else {
        return if has_key {
            Ok(Caller::Service)
        } else {
            Err((401, "unauthorized: send the API key as `X-Api-Key: <key>` (on enclave.host the gateway consumes `Authorization`, so a bearer never arrives here)".into()))
        };
    };
    if let Some(tok) = sso_token_of(req) {
        return match sso::verify(sso_cfg, &tok, now_secs()) {
            Ok(c) => Ok(Caller::User { sub: c.sub, via: "sso" }),
            Err(e) => Err((401, format!("[sso_required] {e}"))),
        };
    }
    // the service path: only a real key counts (an open deployment has no
    // service to trust), and the name must be one of the platform's shapes
    if has_key && cfg.api_key.is_some() {
        return match header(req, "x-user").map(|u| u.trim().to_string()).filter(|u| !u.is_empty()) {
            Some(u) => match sso::canonical_sub(&u) {
                Some(sub) => Ok(Caller::User { sub, via: "key" }),
                None => Err((400, "X-User must be an Enclave identity: a 0x wallet address or an acct_ id".into())),
            },
            None => Err((401, "[sso_required] this is a per-user notebook: name the user with `X-Sso-Token: <Enclave sign-in token>`, or `X-User: <sub>` beside the API key".into())),
        };
    }
    Err((401, "[sso_required] this is a per-user notebook: sign in with your Enclave account and send the token as `X-Sso-Token`".into()))
}

/// The deployment's own public origin, for the tool schemas: what the
/// browser or agent dialled, as the gateway forwarded it.
fn origin_of(req: &IncomingRequest) -> String {
    let authority = req.authority().unwrap_or_else(|| "localhost".to_string());
    let host = authority.split(':').next().unwrap_or("");
    let local = host == "localhost" || host.parse::<std::net::IpAddr>().is_ok();
    let https = match req.scheme() {
        Some(Scheme::Https) => true,
        Some(Scheme::Http) => !local,
        _ => !local,
    };
    format!("{}://{authority}", if https { "https" } else { "http" })
}

// ---- the store, from config ------------------------------------------------

struct Store<'a> {
    ep: s3::Endpoint,
    cfg: &'a Config,
    /// every key this caller can reach starts with this: the configured
    /// prefix, plus `users/<sub>/` in per-user mode
    ns: String,
    /// sealed notes, when the deployment has a master key
    cipher: Option<crypt::Cipher>,
}

/// What a note looks like once fetched and, if sealed, opened.
struct Note {
    text: Vec<u8>,
    etag: String,
    modified: String,
}

impl<'a> Store<'a> {
    fn open(cfg: &'a Config, caller: &Caller) -> Result<Store<'a>, String> {
        if let Some(e) = &cfg.error {
            return Err(e.clone());
        }
        if !cfg.missing.is_empty() {
            return Err(format!(
                "configuration incomplete: {} not set. Set the deployment's config/secrets and restart.",
                cfg.missing.join(", ")
            ));
        }
        let ep = s3::Endpoint::parse(&cfg.endpoint, &cfg.region)?;
        let (ns, scope) = match caller {
            Caller::Service => (cfg.prefix.clone(), "shared".to_string()),
            Caller::User { sub, .. } => (format!("{}users/{}/", cfg.prefix, sub), format!("user:{sub}")),
        };
        let cipher = cfg.master_key.as_deref().map(|m| crypt::Cipher::for_scope(m, &scope));
        Ok(Store { ep, cfg, ns, cipher })
    }
    fn client(&self) -> s3::Client<'_> {
        s3::Client { ep: &self.ep, bucket: &self.cfg.bucket, creds: self.cfg.creds.as_ref() }
    }
    fn key(&self, name: &str) -> String {
        format!("{}{}", self.ns, name)
    }
    fn name_of<'k>(&self, key: &'k str) -> &'k str {
        key.strip_prefix(self.ns.as_str()).unwrap_or(key)
    }

    /// GET and, when sealed, open one note. A plaintext object under an
    /// encrypting deployment still reads (it was readable anyway); a sealed
    /// object under a deployment with no master key does not.
    fn fetch(&self, key: &str) -> Result<Option<Note>, s3::S3Error> {
        let Some(f) = self.client().get(key)? else { return Ok(None) };
        let text = if crypt::Cipher::is_sealed(&f.body) {
            match &self.cipher {
                Some(c) => c.open(key, &f.body).map_err(|e| s3::S3Error::Transport(e))?,
                None => return Err(s3::S3Error::Transport(
                    "this note is sealed but the deployment has no master_key: set the secret and restart".into(),
                )),
            }
        } else {
            f.body
        };
        Ok(Some(Note { text, etag: f.etag, modified: f.modified }))
    }

    /// Seal (when configured) and PUT one note.
    fn store(&self, key: &str, name: &str, text: &[u8], if_match: Option<&str>) -> Result<String, s3::S3Error> {
        match &self.cipher {
            Some(c) => {
                let sealed = c.seal(key, text).map_err(s3::S3Error::Transport)?;
                self.client().put(key, &sealed, "application/octet-stream", if_match)
            }
            None => self.client().put(key, text, content_type_for(name), if_match),
        }
    }
}

fn content_type_for(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("") {
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        _ => "text/plain; charset=utf-8",
    }
}

// ---- handlers --------------------------------------------------------------

fn handle_status(out: ResponseOutparam, cfg: &Config, req: &IncomingRequest) {
    let mut v = serde_json::json!({
        "app": APP, "version": VERSION, "title": cfg.title,
        "configured": cfg.missing.is_empty() && cfg.error.is_none(),
        "missing": cfg.missing,
        "auth": cfg.api_key.is_some(),
        "readOnly": cfg.read_only,
        "users": cfg.sso.is_some(),
        "encrypted": cfg.master_key.is_some(),
    });
    if let Some(e) = &cfg.error {
        v["error"] = serde_json::json!(e);
    }
    if let Some(sc) = &cfg.sso {
        // public facts the UI needs before anyone is signed in
        v["sso"] = serde_json::json!({ "authorize_url": sc.authorize_url, "aud": sc.audience, "accept": sc.accept.len() });
    }
    // who the request already is, when it says: the UI's "signed in as"
    let with_key = authorized(cfg, req) && cfg.api_key.is_some();
    if let Ok(Caller::User { sub, via }) = identify(cfg, req) {
        v["you"] = serde_json::json!({ "sub": sub, "via": via });
    }
    if with_key || (cfg.api_key.is_none() && cfg.sso.is_none()) {
        v["endpoint"] = serde_json::json!(cfg.endpoint);
        v["region"] = serde_json::json!(cfg.region);
        v["bucket"] = serde_json::json!(cfg.bucket);
        v["prefix"] = serde_json::json!(cfg.prefix);
        v["signed"] = serde_json::json!(cfg.creds.is_some());
    }
    json(out, 200, v);
}

/// The notebook, described as tools. Two dialects of the same six verbs:
/// OpenAI function schemas (LangChain, the OpenAI SDK, anything that binds
/// tools that way) and a ready-to-paste `tools.http` block for an eyesoff-ai
/// deployment, whose server-side registry calls plain HTTP endpoints with
/// `{arg}` URL placeholders and `$SECRET` headers.
fn handle_tools(out: ResponseOutparam, origin: &str, users: bool) {
    let name_p = serde_json::json!({ "type": "string", "description": "note name: a relative path like 'projects/enclave.md'; letters, digits, - _ . and spaces, slashes between segments" });
    let content_p = serde_json::json!({ "type": "string", "description": "the note's full text (markdown is a good default)" });
    let prefix_p = serde_json::json!({ "type": "string", "description": "only names starting with this, e.g. 'projects/'" });
    let f = |name: &str, desc: &str, props: serde_json::Value, required: &[&str]| {
        serde_json::json!({ "type": "function", "function": { "name": name, "description": desc,
            "parameters": { "type": "object", "properties": props, "required": required } } })
    };
    let openai = vec![
        f("notes_list", "List the notes in the notebook (name, size, last modified). Call this first when you are not sure what has been written down.",
          serde_json::json!({ "prefix": prefix_p }), &[]),
        f("notes_read", "Read one note's full text by name.", serde_json::json!({ "name": name_p }), &["name"]),
        f("notes_write", "Create or replace a note. Use notes_append to add to an existing note without rewriting it.",
          serde_json::json!({ "name": name_p, "content": content_p }), &["name", "content"]),
        f("notes_append", "Append a paragraph to a note (creating it if needed). The right verb for logging something you learned.",
          serde_json::json!({ "name": name_p, "content": content_p }), &["name", "content"]),
        f("notes_search", "Case-insensitive substring search across all note bodies; returns matching lines with their note names.",
          serde_json::json!({ "query": { "type": "string", "description": "text to look for" }, "prefix": prefix_p }), &["query"]),
        f("notes_delete", "Delete a note by name.", serde_json::json!({ "name": name_p }), &["name"]),
    ];
    let h = |name: &str, desc: &str, method: &str, url: &str, params: serde_json::Value, required: &[&str], body: Option<serde_json::Value>| {
        let mut e = serde_json::json!({ "name": name, "description": desc, "method": method,
            "url": format!("{origin}{url}"),
            "headers": { "x-api-key": "$JOT_API_KEY" },
            "parameters": { "type": "object", "properties": params, "required": required } });
        if users {
            // eyesoff-ai fills $user from the sign-in it verified for the
            // turn, never from the model; without a signed-in caller the
            // tool call fails there instead of reaching here nameless
            e["headers"]["x-user"] = serde_json::json!("$user");
        }
        if let Some(b) = body { e["body"] = b; }
        e
    };
    let eyesoff = serde_json::json!({ "tools": { "http": [
        h("notes_list", "List the notes in the notebook (name, size, last modified).", "GET", "/api/notes", serde_json::json!({ "prefix": prefix_p }), &[], None),
        h("notes_read", "Read one note's full text by name.", "GET", "/api/notes/{name}", serde_json::json!({ "name": name_p }), &["name"], None),
        h("notes_write", "Create or replace a note.", "PUT", "/api/notes/{name}", serde_json::json!({ "name": name_p, "content": content_p }), &["name", "content"], Some(serde_json::json!({ "content": "$content" }))),
        h("notes_append", "Append a paragraph to a note, creating it if needed.", "POST", "/api/notes/{name}/append", serde_json::json!({ "name": name_p, "content": content_p }), &["name", "content"], Some(serde_json::json!({ "content": "$content" }))),
        h("notes_search", "Case-insensitive substring search across note bodies.", "GET", "/api/search", serde_json::json!({ "q": { "type": "string", "description": "text to look for" }, "prefix": prefix_p }), &["q"], None),
        h("notes_delete", "Delete a note by name.", "DELETE", "/api/notes/{name}", serde_json::json!({ "name": name_p }), &["name"], None),
    ] } });
    json(out, 200, serde_json::json!({
        "base_url": origin,
        "auth": if users {
            "X-Api-Key: <api_key> plus X-User: <sub> (a service naming the user), or X-Sso-Token: <Enclave sign-in token>"
        } else {
            "X-Api-Key: <api_key> (Authorization: Bearer works only off-platform: the enclave.host gateway consumes that header)"
        },
        "users": users,
        "openai": openai,
        "eyesoff_ai": eyesoff,
    }));
}

fn handle_list(out: ResponseOutparam, store: &Store, q: &[(String, String)]) {
    let sub = query_get(q, "prefix").unwrap_or("").trim_start_matches('/');
    let limit = query_get(q, "limit").and_then(|l| l.parse().ok()).unwrap_or(LIST_DEFAULT).clamp(1, LIST_MAX);
    let full = format!("{}{}", store.ns, sub);
    match store.client().list(&full, limit) {
        Ok((objects, truncated)) => {
            let notes: Vec<serde_json::Value> = objects
                .iter()
                .filter(|o| !o.key.ends_with('/'))
                .map(|o| serde_json::json!({
                    "name": store.name_of(&o.key), "size": o.size,
                    "modified": o.modified, "etag": o.etag }))
                .collect();
            json(out, 200, serde_json::json!({ "notes": notes, "truncated": truncated, "prefix": sub }));
        }
        Err(e) => s3_err(out, e),
    }
}

fn handle_read(out: ResponseOutparam, store: &Store, name: &str, raw: bool) {
    match store.fetch(&store.key(name)) {
        Ok(Some(f)) => {
            if raw {
                return respond(out, 200, content_type_for(name), &f.text);
            }
            let content = String::from_utf8_lossy(&f.text).into_owned();
            json(out, 200, serde_json::json!({ "name": name, "content": content, "size": f.text.len(), "etag": f.etag, "modified": f.modified }));
        }
        Ok(None) => json_err(out, 404, "no such note"),
        Err(e) => s3_err(out, e),
    }
}

/// The write body: JSON `{content, ifMatch?}` when the request says JSON,
/// otherwise the bytes are the note.
fn parse_write(req: &IncomingRequest, body: Vec<u8>) -> Result<(String, Option<String>), String> {
    let ctype = header(req, "content-type").unwrap_or_default().to_ascii_lowercase();
    if ctype.starts_with("application/json") {
        let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| format!("invalid JSON body: {e}"))?;
        let content = v.get("content").and_then(|c| c.as_str()).ok_or("body needs a string \"content\"")?;
        let if_match = v.get("ifMatch").and_then(|c| c.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
        Ok((content.to_string(), if_match))
    } else {
        let if_match = header(req, "if-match").map(|s| s.trim_matches('"').to_string());
        Ok((String::from_utf8(body).map_err(|_| "note body must be UTF-8 text")?, if_match))
    }
}

fn handle_write(out: ResponseOutparam, store: &Store, name: &str, content: &str, if_match: Option<&str>) {
    if content.len() > MAX_NOTE {
        return json_err(out, 413, "a note caps at 1 MiB");
    }
    match store.store(&store.key(name), name, content.as_bytes(), if_match) {
        Ok(etag) => json(out, 200, serde_json::json!({ "ok": true, "name": name, "size": content.len(), "etag": etag })),
        Err(s3::S3Error::Http(412, _)) => json_err(out, 412, "the note changed since it was read (ETag mismatch); read it again and retry"),
        Err(e) => s3_err(out, e),
    }
}

fn handle_append(out: ResponseOutparam, store: &Store, name: &str, content: &str) {
    let key = store.key(name);
    let (mut text, etag) = match store.fetch(&key) {
        Ok(Some(f)) => (String::from_utf8_lossy(&f.text).into_owned(), Some(f.etag)),
        Ok(None) => (String::new(), None),
        Err(e) => return s3_err(out, e),
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(content);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    if text.len() > MAX_NOTE {
        return json_err(out, 413, "appending would take the note past 1 MiB");
    }
    // conditional on what was just read: two writers appending at once
    // lose nothing, one of them gets a 409 and retries
    let cond = etag.as_deref().filter(|e| !e.is_empty());
    match store.store(&key, name, text.as_bytes(), cond) {
        Ok(new_etag) => json(out, 200, serde_json::json!({ "ok": true, "name": name, "size": text.len(), "etag": new_etag })),
        Err(s3::S3Error::Http(412, _)) => json_err(out, 409, "the note changed while appending; retry"),
        Err(e) => s3_err(out, e),
    }
}

fn handle_delete(out: ResponseOutparam, store: &Store, name: &str) {
    match store.client().delete(&store.key(name)) {
        Ok(()) => json(out, 200, serde_json::json!({ "ok": true, "name": name })),
        Err(e) => s3_err(out, e),
    }
}

fn handle_search(out: ResponseOutparam, store: &Store, q: &[(String, String)]) {
    let needle = query_get(q, "q").unwrap_or("").trim().to_lowercase();
    if needle.is_empty() {
        return json_err(out, 400, "q is required");
    }
    let sub = query_get(q, "prefix").unwrap_or("").trim_start_matches('/');
    let limit = query_get(q, "limit").and_then(|l| l.parse().ok()).unwrap_or(SEARCH_DEFAULT).clamp(1, SEARCH_MAX);
    let full = format!("{}{}", store.ns, sub);
    let client = store.client();
    let (objects, more) = match client.list(&full, SEARCH_SCAN) {
        Ok(r) => r,
        Err(e) => return s3_err(out, e),
    };
    let mut hits = Vec::new();
    let mut scanned = 0usize;
    let mut skipped = 0usize;
    let mut truncated = more;
    'notes: for o in &objects {
        if o.key.ends_with('/') {
            continue;
        }
        if o.size > SEARCH_SKIP_OVER {
            skipped += 1;
            continue;
        }
        let f = match store.fetch(&o.key) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => return s3_err(out, e),
        };
        scanned += 1;
        let text = String::from_utf8_lossy(&f.text);
        for (i, line) in text.lines().enumerate() {
            if line.to_lowercase().contains(&needle) {
                let snippet: String = line.trim().chars().take(200).collect();
                hits.push(serde_json::json!({ "name": store.name_of(&o.key), "line": i + 1, "text": snippet }));
                if hits.len() >= limit {
                    truncated = true;
                    break 'notes;
                }
            }
        }
    }
    json(out, 200, serde_json::json!({ "query": needle, "hits": hits, "scanned": scanned, "skipped": skipped, "truncated": truncated }));
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/").to_string();
        let method = req.method();

        match (&method, path.as_str()) {
            (Method::Get, "/") | (Method::Get, "") => {
                return respond(out, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes());
            }
            (Method::Get, "/ping") => return json(out, 200, serde_json::json!({ "ok": true, "pong": true })),
            (Method::Get, "/sso-return") => {
                return respond(out, 200, "text/html; charset=utf-8", SSO_RETURN_HTML.as_bytes());
            }
            _ => {}
        }
        if !path.starts_with("/api/") {
            return json_err(out, 404, "not found; routes: GET /, GET /ping, GET /sso-return, GET /api/status, GET /api/tools, GET /api/notes, GET|PUT|DELETE /api/notes/<name>, POST /api/notes/<name>/append, GET /api/search?q=");
        }

        let cfg = load_config();
        match (&method, path.as_str()) {
            (Method::Get, "/api/tools") => return handle_tools(out, &origin_of(&req), cfg.sso.is_some()),
            (Method::Get, "/api/status") => return handle_status(out, &cfg, &req),
            _ => {}
        }
        let caller = match identify(&cfg, &req) {
            Ok(c) => c,
            Err((status, msg)) => return json_err(out, status, &msg),
        };
        let store = match Store::open(&cfg, &caller) {
            Ok(s) => s,
            Err(msg) => return json_err(out, 503, &msg),
        };
        let q = query_pairs(&pq);

        match (&method, path.as_str()) {
            (Method::Get, "/api/notes") | (Method::Get, "/api/notes/") => return handle_list(out, &store, &q),
            (Method::Get, "/api/search") => return handle_search(out, &store, &q),
            _ => {}
        }

        let Some(rest) = path.strip_prefix("/api/notes/") else {
            return json_err(out, 404, "not found under /api");
        };
        // /api/notes/<name>[/append|/delete]
        let (raw_name, verb) = if let Some(n) = rest.strip_suffix("/append") {
            (n, "append")
        } else if let Some(n) = rest.strip_suffix("/delete") {
            (n, "delete")
        } else {
            (rest, "")
        };
        let Some(name) = pct_decode(raw_name) else { return json_err(out, 400, "bad percent-encoding in the note name") };
        if !valid_name(&name) {
            return json_err(out, 400, "bad note name: use letters, digits, - _ . and spaces, with single slashes between segments (no . or .. segments, at most 200 bytes)");
        }
        let writes = matches!((&method, verb), (Method::Put, "") | (Method::Post, "") | (Method::Post, "append") | (Method::Delete, "") | (Method::Post, "delete"));
        if writes && cfg.read_only {
            return json_err(out, 403, "this deployment is read-only (readOnly in its config)");
        }

        match (&method, verb) {
            (Method::Get, "") => {
                let raw = query_get(&q, "raw").is_some_and(|r| r != "0")
                    || header(&req, "accept").is_some_and(|a| a.starts_with("text/plain"));
                handle_read(out, &store, &name, raw)
            }
            (Method::Put, "") | (Method::Post, "") => {
                let Ok(body) = read_body(&req, MAX_BODY) else { return json_err(out, 413, "request body too large") };
                match parse_write(&req, body) {
                    Ok((content, if_match)) => handle_write(out, &store, &name, &content, if_match.as_deref()),
                    Err(msg) => json_err(out, 400, &msg),
                }
            }
            (Method::Post, "append") => {
                let Ok(body) = read_body(&req, MAX_BODY) else { return json_err(out, 413, "request body too large") };
                match parse_write(&req, body) {
                    Ok((content, _)) => handle_append(out, &store, &name, &content),
                    Err(msg) => json_err(out, 400, &msg),
                }
            }
            (Method::Delete, "") | (Method::Post, "delete") => handle_delete(out, &store, &name),
            _ => json_err(out, 405, "method not allowed for this route"),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_relative_plain_paths() {
        for ok in ["a", "a.md", "projects/enclave.md", "meeting notes 2026-09-01.md", "x/y/z"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "/a", "a/", "a//b", "..", "a/../b", "./a", "a?b", "a#b", "a\\b", "ü.md", &"x".repeat(201)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn query_parsing_decodes() {
        let q = query_pairs("/api/search?q=hello%20world&prefix=a%2Fb&limit=5&flag");
        assert_eq!(query_get(&q, "q"), Some("hello world"));
        assert_eq!(query_get(&q, "prefix"), Some("a/b"));
        assert_eq!(query_get(&q, "limit"), Some("5"));
        assert_eq!(query_get(&q, "flag"), Some(""));
        assert!(query_pairs("/api/notes").is_empty());
        assert_eq!(pct_decode("a%2Fb%20c"), Some("a/b c".into()));
        assert_eq!(pct_decode("%zz"), None);
    }

    #[test]
    fn key_compare_is_exact() {
        assert!(key_matches("abc", "abc"));
        assert!(!key_matches("abc", "abd"));
        assert!(!key_matches("abc", "ab"));
    }
}
