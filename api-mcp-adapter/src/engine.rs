//! The executor: one HTTP entry - the same shape an eyesoff-ai `tools.http`
//! entry has - turned into one outbound request, and its response turned into
//! a tool result.
//!
//! This is a port of eyesoff-ai's `tools.rs` call_http path, kept semantically
//! identical on purpose: an entry moved out of an eyesoff-ai config and into
//! this app's config must behave exactly as it did, argument for argument,
//! header for header. The rules, restated:
//!
//!   - `{arg}` placeholders in the URL are substituted percent-encoded; every
//!     argument the URL did not consume becomes the query string on a GET or
//!     DELETE (or when `query` is true) and the JSON body otherwise;
//!   - a `body` template overrides that: `"$arg"` as a WHOLE string value is
//!     replaced by the argument, a hole left unfilled is pruned (an optional
//!     argument the caller skipped is not sent as the literal "$factor"), and
//!     literal text containing a `$` travels untouched;
//!   - the reserved `"$images"` / `"$image"` holes take the pictures the CALLER
//!     attached under the arguments `images` (array of data URIs) and `image`
//!     (the first one). eyesoff-ai fills those from the turn's attachments,
//!     which is bytes the model could never write into a call itself; any
//!     other MCP client may pass a data URI the same way;
//!   - headers reference secrets by name (`"Bearer $TOOL_KEY"`) and a name
//!     nothing resolves is an error that SAYS WHICH SECRET is missing rather
//!     than a baffling 401 from the endpoint; the reserved whole-value `$user`
//!     carries the caller's identity and fails closed without one;
//!   - `result: {"image": <path>}` extracts a picture (base64 or a data URI)
//!     and `result: {"text": <path>}` extracts one field so the model is
//!     spared the envelope, falling back to the whole body when the path
//!     misses; `sources: {list, title, url}` pulls citation rows out.
//!
//! Everything here that decides WHAT to send is pure and unit-tested on the
//! host; only `call` touches wasi:http.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::bindings::wasi::http::types::Method;
use crate::http::{self, HttpReq};

pub const DEFAULT_TIMEOUT_S: u64 = 20;
/// the cap on ONE response before it is even looked at; the same default
/// eyesoff-ai applies to its own registry
pub const DEFAULT_MAX_BYTES: usize = 256 * 1024;
/// a picture arrives as megabytes of base64 inside JSON, so an image-producing
/// entry that sets no cap of its own gets an image-sized default
pub const IMAGE_MAX_BYTES: usize = 12 * 1024 * 1024;

/// The deployment-wide defaults an entry may override.
#[derive(Clone, Copy)]
pub struct Settings {
    pub timeout_s: u64,
    pub max_bytes: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { timeout_s: DEFAULT_TIMEOUT_S, max_bytes: DEFAULT_MAX_BYTES }
    }
}

/// One HTTP endpoint exposed as a tool. Field for field the eyesoff-ai
/// `tools.http` entry, so a block written for one is a block written for
/// the other. The prompt-side fields (`format`, `route`, `route_arg`,
/// `max_chars`, `group`) are not executed here - they need the model or the
/// conversation - but they are carried through to the client on the tool's
/// `_meta`, so an eyesoff-ai that discovers this server applies them exactly
/// as it would to its own entries.
#[derive(Deserialize, Clone)]
pub struct HttpTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// the switch this endpoint sits under in a client that has switches
    #[serde(default)]
    pub group: Option<String>,
    /// JSON Schema for the arguments (an object schema). Absent = none.
    #[serde(default)]
    pub parameters: Option<serde_json::Value>,
    /// absolute URL, may carry `{arg}` placeholders and `$SECRET` references
    pub url: String,
    /// GET (default) | POST | PUT | PATCH | DELETE
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    /// send leftover arguments as a query string even on a POST
    #[serde(default)]
    pub query: Option<bool>,
    #[serde(default)]
    pub timeout_s: Option<u64>,
    #[serde(default)]
    pub max_bytes: Option<usize>,
    #[serde(default)]
    pub max_chars: Option<usize>,
    #[serde(default)]
    pub result: Option<ResultMap>,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub route_arg: Option<String>,
    #[serde(default)]
    pub sources: Option<SourcesMap>,
}

/// Where a response's citable hits live: `list` names the array, `title` and
/// `url` name fields WITHIN one hit.
#[derive(Deserialize, Clone)]
pub struct SourcesMap {
    pub list: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}

/// Where in a JSON response the result lives, and what it IS. Paths are
/// dot-separated keys and array indexes ("data.0.b64_json").
#[derive(Deserialize, Clone, Default)]
pub struct ResultMap {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// A picture a tool produced, for the client.
#[derive(Debug)]
pub struct Picture {
    pub mime: String,
    pub b64: String,
}

/// What one call produced.
#[derive(Debug)]
pub struct Outcome {
    pub text: String,
    pub sources: Vec<(String, String)>,
    pub image: Option<Picture>,
}

/// The request one call would make, decided before anything is sent.
#[derive(Debug)]
pub struct Plan {
    pub method: Method,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
    pub timeout_s: u64,
    pub max_bytes: usize,
}

impl HttpTool {
    /// The body template asks for the caller's attached pictures.
    pub fn wants_images(&self) -> bool {
        fn scan(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::String(s) => {
                    matches!(s.trim(), "$images" | "$image" | "${images}" | "${image}")
                }
                serde_json::Value::Array(a) => a.iter().any(scan),
                serde_json::Value::Object(o) => o.values().any(scan),
                _ => false,
            }
        }
        self.body.as_ref().is_some_and(scan)
    }

    /// The response carries a picture for the client (see ResultMap).
    pub fn makes_image(&self) -> bool {
        self.result.as_ref().is_some_and(|r| r.image.is_some())
    }

    /// Some header carries the reserved `$user` slot: this endpoint acts for
    /// a named account and must never be reached nameless.
    pub fn requires_user(&self) -> bool {
        self.headers.values().any(|v| is_user_slot(v))
    }

    pub fn method_name(&self) -> String {
        self.method.as_deref().unwrap_or("GET").trim().to_ascii_uppercase()
    }

    /// The parameter a routed line binds to: `route_arg`, or the entry's
    /// sole required parameter.
    pub fn route_binding(&self) -> Option<String> {
        if let Some(a) = self.route_arg.as_deref() {
            return Some(a.to_string());
        }
        let req = self.parameters.as_ref()?.get("required")?.as_array()?;
        if req.len() == 1 {
            return req[0].as_str().map(str::to_string);
        }
        None
    }

    /// The switch this endpoint sits under when it says so, or when it is
    /// about pictures; None = decided among its siblings (`group_of`).
    fn own_group(&self) -> Option<String> {
        match self.group.as_deref().map(str::trim) {
            Some(g) if !g.is_empty() => Some(g.to_string()),
            _ if self.makes_image() || self.wants_images() => Some(GROUP_IMAGES.to_string()),
            _ => None,
        }
    }

    /// The names `required` lists that the arguments do not carry.
    pub fn missing_required(&self, args: &serde_json::Value) -> Vec<String> {
        let Some(req) = self.parameters.as_ref().and_then(|p| p.get("required")).and_then(|r| r.as_array()) else {
            return Vec::new();
        };
        req.iter()
            .filter_map(|r| r.as_str())
            .filter(|r| args.get(r).map_or(true, |v| v.is_null()))
            .map(str::to_string)
            .collect()
    }
}

/// eyesoff-ai's fixed group for everything about pictures.
pub const GROUP_IMAGES: &str = "images";

/// The switch entry `i` sits under, by eyesoff-ai's rule: its own `group`
/// when it names one, "images" when it is about pictures, else the family
/// name its function name shares with at least one sibling (`notes_read`,
/// `notes_write` -> "notes"), else the function name itself.
pub fn group_of(tools: &[HttpTool], i: usize) -> String {
    let t = &tools[i];
    if let Some(g) = t.own_group() {
        return g;
    }
    if let Some(fam) = t.name.split(['_', '-']).next().filter(|f| !f.is_empty() && *f != t.name) {
        let siblings = tools.iter().enumerate().filter(|(j, o)| {
            *j != i && o.own_group().is_none() && o.name.split(['_', '-']).next() == Some(fam)
        });
        if siblings.count() > 0 {
            return fam.to_string();
        }
    }
    t.name.clone()
}

/// Function names a model can actually reproduce: a name with a space or a
/// quote in it comes back mangled and matches nothing.
pub fn check_name(n: &str) -> Result<(), String> {
    if n.is_empty() || n.len() > 64 {
        return Err("a name must be 1-64 characters".into());
    }
    if !n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("a name may only use letters, digits, '_' and '-'".into());
    }
    Ok(())
}

/// Whatever was configured, as an object schema. A model handed a schema that
/// is not an object tends to answer with a bare value the caller cannot bind.
pub fn object_schema(v: Option<&serde_json::Value>) -> serde_json::Value {
    match v {
        Some(v) if v.is_object() => v.clone(),
        _ => serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// The tool as MCP `tools/list` describes it. The `_meta` entry under the
/// `enclave.host/tool` key carries what eyesoff-ai needs beyond the schema:
/// which switch the tool sits under, whether it takes the turn's pictures or
/// returns one, how long a call may take, and the prompt-side settings the
/// client applies to the result. A client that does not know the key
/// ignores it, as the protocol says it should.
pub fn describe(t: &HttpTool, group: &str) -> serde_json::Value {
    let method = t.method_name();
    let mut meta = serde_json::Map::new();
    meta.insert("group".into(), serde_json::json!(group));
    if t.wants_images() {
        meta.insert("images".into(), serde_json::json!(true));
    }
    if t.makes_image() {
        meta.insert("result".into(), serde_json::json!("image"));
    }
    if let Some(s) = t.timeout_s {
        meta.insert("timeout_s".into(), serde_json::json!(s));
    }
    if let Some(n) = t.max_chars {
        meta.insert("max_chars".into(), serde_json::json!(n));
    }
    if let Some(f) = &t.format {
        meta.insert("format".into(), serde_json::json!(f));
    }
    if let Some(r) = &t.route {
        meta.insert("route".into(), serde_json::json!(r));
        if let Some(a) = t.route_binding() {
            meta.insert("route_arg".into(), serde_json::json!(a));
        }
    }
    if t.requires_user() {
        meta.insert("user".into(), serde_json::json!(true));
    }
    serde_json::json!({
        "name": t.name,
        "description": t.description,
        "inputSchema": object_schema(t.parameters.as_ref()),
        "annotations": {
            "readOnlyHint": method == "GET",
            "destructiveHint": method == "DELETE",
            "idempotentHint": matches!(method.as_str(), "GET" | "PUT" | "DELETE"),
            "openWorldHint": true,
        },
        "_meta": { crate::META_KEY: serde_json::Value::Object(meta) },
    })
}

fn is_user_slot(v: &str) -> bool {
    matches!(v.trim(), "$user" | "${user}")
}

/// `$user` is the WHOLE value of a header or it is nothing. A value that
/// merely contains it ("Bearer $user") is a config mistake, and the honest
/// answer is to say so: expand() deliberately leaves the reference alone so
/// it is not misreported as a missing secret, which would otherwise let the
/// literal string "$user" travel to an endpoint as if it were an identity.
fn stray_user(v: &str) -> bool {
    !is_user_slot(v) && (v.contains("$user") || v.contains("${user}"))
}

/// Every `$NAME` / `${NAME}` in `s` whose secret is set, substituted; the
/// first name nothing resolves, reported. The reserved `user` is never a
/// secret name and is left for the identity pass. A `$` that starts no name
/// (a price, a shell literal) is text.
pub fn expand(s: &str, secret: &dyn Fn(&str) -> Option<String>) -> (String, Option<String>) {
    let mut out = String::with_capacity(s.len());
    let mut missing = None;
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let (name, tail, literal) = match after.strip_prefix('{') {
            Some(b) => match b.find('}') {
                Some(j) => (&b[..j], &b[j + 1..], &rest[i..i + 2 + j + 1]),
                None => ("", after, "$"),
            },
            None => {
                let end = after
                    .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(after.len());
                (&after[..end], &after[end..], &rest[i..i + 1 + end])
            }
        };
        if name.is_empty() || name.starts_with(|c: char| c.is_ascii_digit()) {
            // not a reference: keep the `$` and carry on after it
            out.push('$');
            rest = after;
            continue;
        }
        if name == "user" {
            out.push_str(literal);
        } else {
            match secret(name) {
                Some(v) => out.push_str(&v),
                None => {
                    if missing.is_none() {
                        missing = Some(name.to_string());
                    }
                    out.push_str(literal);
                }
            }
        }
        rest = tail;
    }
    out.push_str(rest);
    (out, missing)
}

/// The headers one call sends: the `$user` slot filled from the caller's
/// identity (or the call refused, when there is none), then secrets
/// substituted, then a header whose secret is not set refused by name.
pub fn resolve_headers(
    t: &HttpTool,
    user: Option<&str>,
    secret: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    for (k, v) in &t.headers {
        if stray_user(v) {
            return Err(format!(
                "the {} tool's '{k}' header mixes the caller's identity into a larger value \
                 ({v:?}). The $user slot is the WHOLE value of a header or nothing - fix \
                 the deployment's config. The call was not made.",
                t.name
            ));
        }
        if is_user_slot(v) {
            match user {
                Some(u) => out.push((k.to_ascii_lowercase(), u.to_string())),
                None => {
                    return Err(format!(
                        "the {} tool acts on behalf of the signed-in user (its '{k}' header is \
                         $user), and this call names no user. Sign in with Enclave, or call with \
                         the API key and an X-User header naming the account. Do not retry \
                         without one.",
                        t.name
                    ))
                }
            }
            continue;
        }
        let (val, missing) = expand(v, secret);
        if let Some(name) = missing {
            return Err(format!(
                "the {} tool's '{k}' header references ${name} but no such secret is set on \
                 this deployment - add {name} to the deployment's secrets (console or \
                 set_secrets) and restart it to apply. The call was not made.",
                t.name
            ));
        }
        out.push((k.to_ascii_lowercase(), val));
    }
    Ok(out)
}

/// Decide the request: method, URL, headers and body, from the entry and the
/// arguments. Pure, so the whole templating contract is testable on the host.
pub fn plan(
    t: &HttpTool,
    s: Settings,
    args: &serde_json::Value,
    user: Option<&str>,
    secret: &dyn Fn(&str) -> Option<String>,
) -> Result<Plan, String> {
    let empty = serde_json::Map::new();
    let obj = args.as_object().unwrap_or(&empty);
    let method = match t.method_name().as_str() {
        "GET" => Method::Get,
        "POST" => Method::Post,
        "PUT" => Method::Put,
        "PATCH" => Method::Patch,
        "DELETE" => Method::Delete,
        other => return Err(format!("tool '{}' has an unsupported method '{other}'", t.name)),
    };
    let is_get = matches!(method, Method::Get | Method::Delete);
    if stray_user(&t.url) {
        return Err(format!(
            "tool '{}' puts the caller's identity in its url. $user is a header value, \
             never part of a URL - a path or a query string is not where an identity \
             belongs, and it would travel in logs and referrers. Fix the config.",
            t.name
        ));
    }
    let (base, missing) = expand(&t.url, secret);
    if let Some(name) = missing {
        return Err(format!(
            "the {} tool's url references ${name} but no such secret is set on this \
             deployment - add {name} to the deployment's secrets and restart it to apply.",
            t.name
        ));
    }
    if !base.starts_with("http://") && !base.starts_with("https://") {
        return Err(format!("tool '{}' has a url that is not absolute http(s): {base}", t.name));
    }
    // {arg} in the URL is consumed by the path; whatever is left goes to the
    // query string (GET) or the body (POST)
    let (mut url, used) = substitute(&base, obj);
    let leftover: Vec<(&String, &serde_json::Value)> = obj
        .iter()
        .filter(|(k, _)| !used.contains(&k.as_str()))
        // the reserved picture arguments never ride a query string: they are
        // for body templates, and megabytes of base64 in a URL is a 414
        .filter(|(k, _)| !(t.wants_images() && (k.as_str() == "image" || k.as_str() == "images")))
        .collect();
    let as_query = t.query.unwrap_or(is_get);
    if as_query && !leftover.is_empty() {
        let mut q = String::new();
        for (k, v) in &leftover {
            q.push(if q.is_empty() { '?' } else { '&' });
            if url.contains('?') && q.len() == 1 {
                q.pop();
                q.push('&');
            }
            q.push_str(&pct(k));
            q.push('=');
            q.push_str(&pct(&scalar(v)));
        }
        url.push_str(&q);
    }
    let body: Option<Vec<u8>> = if is_get {
        None
    } else {
        let payload = match &t.body {
            Some(tpl) => prune_unfilled(fill_template(tpl, obj), t),
            None if as_query => serde_json::json!({}),
            None => args.clone(),
        };
        Some(payload.to_string().into_bytes())
    };
    Ok(Plan {
        method,
        url,
        headers: resolve_headers(t, user, secret)?,
        body,
        timeout_s: t.timeout_s.unwrap_or(s.timeout_s),
        max_bytes: req_max(t, s),
    })
}

/// A response cap that fits what the tool RETURNS.
fn req_max(t: &HttpTool, s: Settings) -> usize {
    match t.max_bytes {
        Some(n) => n,
        None if t.makes_image() => IMAGE_MAX_BYTES.max(s.max_bytes),
        None => s.max_bytes,
    }
}

/// Make the call: plan it, send it, shape the response.
pub fn call(
    t: &HttpTool,
    s: Settings,
    args: &serde_json::Value,
    user: Option<&str>,
    secret: &dyn Fn(&str) -> Option<String>,
) -> Result<Outcome, String> {
    let p = plan(t, s, args, user, secret)?;
    let mut req = HttpReq::get(&p.url);
    req.method = p.method;
    req.timeout_s = p.timeout_s;
    req.max_bytes = p.max_bytes;
    req.body = p.body.as_deref();
    req = req.header("accept", b"application/json, text/plain;q=0.9, */*;q=0.8");
    if p.body.is_some() {
        req = req.header("content-type", b"application/json");
    }
    for (k, v) in &p.headers {
        req = req.header(k, v.as_bytes());
    }
    let r = http::request(req)?;
    finish(t, r.status, &r.body, r.truncated, p.max_bytes, args)
}

/// The response, shaped: an HTTP error is an error with a hint of the body,
/// a cut-off body is a result that says so, and otherwise the sources and
/// the result map apply.
pub fn finish(
    t: &HttpTool,
    status: u16,
    body: &[u8],
    truncated: bool,
    max_bytes: usize,
    args: &serde_json::Value,
) -> Result<Outcome, String> {
    let text = String::from_utf8_lossy(body).trim().to_string();
    if status >= 400 {
        let hint: String = text.chars().take(400).collect();
        return Err(format!("tool '{}' returned HTTP {status}: {hint}", t.name));
    }
    if truncated {
        return Ok(Outcome {
            text: format!("{text}\n[response was cut off at {max_bytes} bytes]"),
            sources: Vec::new(),
            image: None,
        });
    }
    let mut sources = Vec::new();
    extract_sources(t, &text, &mut sources);
    let (text, image) = map_result(t, text, args)?;
    Ok(Outcome { text, sources, image })
}

/// The hits a sources map names, as (title, url) rows. Tolerant: a path that
/// misses yields no sources, never an error.
pub fn extract_sources(t: &HttpTool, text: &str, sources: &mut Vec<(String, String)>) {
    let Some(sm) = &t.sources else { return };
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) else { return };
    let Some(arr) = json_path(&parsed, &sm.list).and_then(|v| v.as_array()) else { return };
    for hit in arr {
        let field = |p: &Option<String>| -> Option<String> {
            p.as_deref()
                .and_then(|p| json_path(hit, p))
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let title = field(&sm.title);
        let url = field(&sm.url);
        if title.is_none() && url.is_none() {
            continue;
        }
        let u = url.clone().unwrap_or_default();
        sources.push((title.or(url).unwrap_or_default(), u));
    }
}

/// The response, shaped by the tool's ResultMap.
fn map_result(
    t: &HttpTool,
    text: String,
    args: &serde_json::Value,
) -> Result<(String, Option<Picture>), String> {
    let Some(rm) = &t.result else { return Ok((text, None)) };
    let parsed: Option<serde_json::Value> = serde_json::from_str(&text).ok();
    if let Some(path) = &rm.image {
        let raw = parsed
            .as_ref()
            .and_then(|j| json_path(j, path))
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                let hint: String = text.chars().take(200).collect();
                format!(
                    "tool '{}' answered without an image at result.image path '{path}': {hint}",
                    t.name
                )
            })?;
        let (mime, b64) = image_payload(raw);
        let note = format!("Image generated (request: \"{}\").", image_request_label(args));
        return Ok((note, Some(Picture { mime, b64 })));
    }
    if let Some(path) = &rm.text {
        if let Some(v) = parsed.as_ref().and_then(|j| json_path(j, path)) {
            return Ok((
                match v {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                },
                None,
            ));
        }
    }
    Ok((text, None))
}

/// What a picture was made from: the `prompt` argument, or the other
/// arguments minus any picture payloads (kilobytes of base64 are not a
/// "request" anyone should read back).
pub fn image_request_label(args: &serde_json::Value) -> String {
    if let Some(p) = args.get("prompt").and_then(|p| p.as_str()) {
        return p.to_string();
    }
    match args {
        serde_json::Value::Object(o) => {
            let slim: serde_json::Map<String, serde_json::Value> = o
                .iter()
                .filter(|(k, _)| k.as_str() != "image" && k.as_str() != "images")
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            truncate(&serde_json::Value::Object(slim).to_string(), 200)
        }
        other => truncate(&other.to_string(), 200),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n[truncated at {max} characters]")
}

/// Walk a dot path ("data.0.b64_json") through keys and array indexes.
pub fn json_path<'a>(v: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match seg.parse::<usize>() {
            Ok(i) => cur.get(i)?,
            Err(_) => cur.get(seg)?,
        };
    }
    Some(cur)
}

/// (mime, base64) from a field that may be raw base64 or a full data URI.
pub fn image_payload(raw: &str) -> (String, String) {
    if let Some(rest) = raw.strip_prefix("data:") {
        if let Some((meta, b64)) = rest.split_once(',') {
            let mime = meta.split(';').next().unwrap_or("").trim();
            let mime = if mime.is_empty() { "image/png" } else { mime };
            return (mime.to_string(), b64.to_string());
        }
    }
    ("image/png".to_string(), raw.to_string())
}

/// A scalar argument as a bare string: JSON strings lose their quotes, numbers
/// and booleans print themselves, anything structured stays JSON.
fn scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Replace `{name}` in a URL with the matching argument, percent-encoded.
/// Returns the names it consumed so they are not ALSO sent as query params.
fn substitute<'a>(
    url: &str,
    obj: &'a serde_json::Map<String, serde_json::Value>,
) -> (String, Vec<&'a str>) {
    let mut out = String::with_capacity(url.len());
    let mut used = Vec::new();
    let mut rest = url;
    while let Some(i) = rest.find('{') {
        let Some(j) = rest[i..].find('}') else { break };
        let key = &rest[i + 1..i + j];
        match obj.get_key_value(key) {
            Some((k, v)) => {
                out.push_str(&rest[..i]);
                out.push_str(&pct(&scalar(v)));
                used.push(k.as_str());
            }
            None => out.push_str(&rest[..i + j + 1]),
        }
        rest = &rest[i + j + 1..];
    }
    out.push_str(rest);
    (out, used)
}

/// Drop template holes the fill left behind, so a declared-but-omitted
/// argument means "send nothing" instead of sending the literal "$name".
/// Only a WHOLE string value naming one of the tool's DECLARED parameters
/// (or the reserved image slots) is a hole - literal text containing a `$`
/// still travels untouched.
fn prune_unfilled(v: serde_json::Value, t: &HttpTool) -> serde_json::Value {
    fn hole(v: &serde_json::Value, t: &HttpTool) -> bool {
        let Some(name) = v.as_str().and_then(|s| s.strip_prefix('$')) else { return false };
        let name = name.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(name);
        if matches!(name, "image" | "images") {
            return true;
        }
        t.parameters
            .as_ref()
            .and_then(|p| p.get("properties"))
            .and_then(|p| p.as_object())
            .is_some_and(|props| props.contains_key(name))
    }
    match v {
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.into_iter()
                .filter(|(_, val)| !hole(val, t))
                .map(|(k, val)| (k, prune_unfilled(val, t)))
                .collect(),
        ),
        serde_json::Value::Array(a) => serde_json::Value::Array(
            a.into_iter().filter(|x| !hole(x, t)).map(|x| prune_unfilled(x, t)).collect(),
        ),
        other => other,
    }
}

/// Fill `"$arg"` holes in a body template. Only a WHOLE string value is
/// replaced, so a template can carry literal text containing a `$` safely.
fn fill_template(
    tpl: &serde_json::Value,
    obj: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    match tpl {
        serde_json::Value::String(s) => {
            let name = s.strip_prefix('$').map(|r| {
                r.strip_prefix('{').and_then(|x| x.strip_suffix('}')).unwrap_or(r).to_string()
            });
            match name.and_then(|n| obj.get(&n).cloned()) {
                Some(v) => v,
                None => tpl.clone(),
            }
        }
        serde_json::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(|x| fill_template(x, obj)).collect())
        }
        serde_json::Value::Object(o) => serde_json::Value::Object(
            o.iter().map(|(k, v)| (k.clone(), fill_template(v, obj))).collect(),
        ),
        other => other.clone(),
    }
}

pub fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(v: serde_json::Value) -> HttpTool {
        serde_json::from_value(v).unwrap()
    }

    fn secrets<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |n| pairs.iter().find(|(k, _)| *k == n).map(|(_, v)| v.to_string())
    }

    fn body_json(p: &Plan) -> serde_json::Value {
        serde_json::from_slice(p.body.as_deref().unwrap()).unwrap()
    }

    /// The URL contract: `{arg}` is consumed and percent-encoded, the rest
    /// rides the query on a GET, and a POST carries the arguments as JSON.
    #[test]
    fn url_placeholders_and_leftovers() {
        let t = tool(serde_json::json!({ "name": "r", "url": "https://h/api/notes/{name}" }));
        let none = secrets(&[]);
        let p = plan(&t, Settings::default(), &serde_json::json!({ "name": "a/b c.md", "raw": 1 }), None, &none).unwrap();
        assert_eq!(p.url, "https://h/api/notes/a%2Fb%20c.md?raw=1");
        assert!(matches!(p.method, Method::Get));
        assert!(p.body.is_none());
        // a POST without a template sends the arguments as-is
        let t = tool(serde_json::json!({ "name": "w", "url": "https://h/x", "method": "POST" }));
        let p = plan(&t, Settings::default(), &serde_json::json!({ "a": 1, "b": "two" }), None, &none).unwrap();
        assert_eq!(body_json(&p), serde_json::json!({ "a": 1, "b": "two" }));
        assert_eq!(p.url, "https://h/x");
        // `query: true` on a POST moves them to the URL and sends `{}`
        let t = tool(serde_json::json!({ "name": "w", "url": "https://h/x?v=2", "method": "POST", "query": true }));
        let p = plan(&t, Settings::default(), &serde_json::json!({ "a": 1 }), None, &none).unwrap();
        assert_eq!(p.url, "https://h/x?v=2&a=1");
        assert_eq!(body_json(&p), serde_json::json!({}));
    }

    /// The body template contract: whole-value `$arg` holes are filled, an
    /// unfilled declared hole is pruned, literal `$` text survives, and the
    /// reserved picture holes take the caller's attachments.
    #[test]
    fn body_templates_fill_prune_and_keep_literals() {
        let t = tool(serde_json::json!({
            "name": "gen", "url": "https://h/v1/images", "method": "POST",
            "parameters": { "type": "object", "properties": { "prompt": {}, "size": {} }, "required": ["prompt"] },
            "body": { "prompt": "$prompt", "n": 1, "size": "$size", "note": "costs $5", "pics": "$images" }
        }));
        let none = secrets(&[]);
        let p = plan(&t, Settings::default(), &serde_json::json!({ "prompt": "a cat" }), None, &none).unwrap();
        assert_eq!(body_json(&p), serde_json::json!({ "prompt": "a cat", "n": 1, "note": "costs $5" }));
        assert!(t.wants_images());
        let p = plan(
            &t, Settings::default(),
            &serde_json::json!({ "prompt": "a cat", "size": "1024x768", "images": ["data:image/png;base64,AA=="], "image": "data:image/png;base64,AA==" }),
            None, &none,
        ).unwrap();
        let b = body_json(&p);
        assert_eq!(b["size"], "1024x768");
        assert_eq!(b["pics"], serde_json::json!(["data:image/png;base64,AA=="]));
        // the picture arguments never leak into the URL of an image tool
        assert_eq!(p.url, "https://h/v1/images");
        // an image-producing entry gets the image-sized response cap
        let t = tool(serde_json::json!({ "name": "g", "url": "https://h/x", "method": "POST", "result": { "image": "data.0.b64_json" } }));
        assert_eq!(plan(&t, Settings::default(), &serde_json::json!({}), None, &none).unwrap().max_bytes, IMAGE_MAX_BYTES);
    }

    /// Secrets resolve by name in headers and the URL; a missing one names
    /// itself instead of reaching the endpoint as a literal.
    #[test]
    fn secrets_expand_or_name_the_gap() {
        let s = secrets(&[("TOOL_KEY", "k-123"), ("BASE", "https://api.example")]);
        assert_eq!(expand("Bearer $TOOL_KEY", &s), ("Bearer k-123".to_string(), None));
        assert_eq!(expand("${BASE}/v1", &s), ("https://api.example/v1".to_string(), None));
        assert_eq!(expand("price $5 and $ sign", &s), ("price $5 and $ sign".to_string(), None));
        assert_eq!(expand("$user", &s), ("$user".to_string(), None));
        assert_eq!(expand("x $NOPE y", &s).1.as_deref(), Some("NOPE"));
        let t = tool(serde_json::json!({
            "name": "a", "url": "${BASE}/v1/x", "headers": { "Authorization": "Bearer $TOOL_KEY" }
        }));
        let p = plan(&t, Settings::default(), &serde_json::json!({}), None, &s).unwrap();
        assert_eq!(p.url, "https://api.example/v1/x");
        assert_eq!(p.headers, vec![("authorization".to_string(), "Bearer k-123".to_string())]);
        let t = tool(serde_json::json!({ "name": "a", "url": "https://h/x", "headers": { "x-key": "$MISSING" } }));
        let e = plan(&t, Settings::default(), &serde_json::json!({}), None, &s).unwrap_err();
        assert!(e.contains("$MISSING") && e.contains("no such secret"), "{e}");
        let t = tool(serde_json::json!({ "name": "a", "url": "${NOWHERE}/x" }));
        let e = plan(&t, Settings::default(), &serde_json::json!({}), None, &s).unwrap_err();
        assert!(e.contains("$NOWHERE"), "{e}");
    }

    /// The `$user` slot fills from the caller's identity and fails closed
    /// without one; and it is the WHOLE value of a header or nothing.
    #[test]
    fn user_slot_fills_or_fails_closed() {
        let t = tool(serde_json::json!({ "name": "notes_read", "url": "https://h/x", "headers": { "x-user": "$user", "x-api-key": "k" } }));
        assert!(t.requires_user());
        let none = secrets(&[]);
        let h = resolve_headers(&t, Some("0xabc"), &none).unwrap();
        assert_eq!(h, vec![("x-api-key".to_string(), "k".to_string()), ("x-user".to_string(), "0xabc".to_string())]);
        let e = resolve_headers(&t, None, &none).unwrap_err();
        assert!(e.contains("notes_read") && e.contains("signed-in user"), "{e}");
        assert!(!tool(serde_json::json!({ "name": "a", "url": "https://h" })).requires_user());

        // ...and the slot is the WHOLE value or nothing: a value that merely
        // MENTIONS it would otherwise send the literal "$user" as if it were
        // an identity, which is exactly the confusion the slot prevents
        let mixed = tool(serde_json::json!({ "name": "n", "url": "https://h", "headers": { "authorization": "Bearer $user" } }));
        let e = resolve_headers(&mixed, Some("0xabc"), &none).unwrap_err();
        assert!(e.contains("WHOLE value"), "{e}");
        assert!(resolve_headers(&mixed, None, &none).is_err(), "and without a caller too");
        // an identity has no business in a URL either
        let in_url = tool(serde_json::json!({ "name": "n", "url": "https://h/u/$user" }));
        let e = plan(&in_url, Settings::default(), &serde_json::json!({}), Some("0xabc"), &none).unwrap_err();
        assert!(e.contains("never part of a URL"), "{e}");
        // a header that merely says the WORD user is untouched: the rule is
        // about the reference, not the noun
        let word = tool(serde_json::json!({ "name": "n", "url": "https://h", "headers": { "x-note": "for the user only" } }));
        assert!(resolve_headers(&word, None, &none).is_ok());
    }

    /// Results: an image path yields a picture for the client and a short
    /// note; a text path spares the model the envelope; a miss falls back to
    /// the body; HTTP errors carry a hint; a cut-off body says so.
    #[test]
    fn results_are_shaped_by_the_map() {
        let t = tool(serde_json::json!({ "name": "gen", "url": "https://h", "result": { "image": "data.0.b64_json" } }));
        let body = serde_json::json!({ "data": [{ "b64_json": "AAAA" }] }).to_string();
        let o = finish(&t, 200, body.as_bytes(), false, 1, &serde_json::json!({ "prompt": "a cat" })).unwrap();
        let img = o.image.unwrap();
        assert_eq!((img.mime.as_str(), img.b64.as_str()), ("image/png", "AAAA"));
        assert!(o.text.contains("a cat"));
        // a data URI carries its own mime
        let body = serde_json::json!({ "data": [{ "b64_json": "data:image/webp;base64,BBBB" }] }).to_string();
        let o = finish(&t, 200, body.as_bytes(), false, 1, &serde_json::json!({ "factor": 2, "image": "data:…" })).unwrap();
        let img = o.image.unwrap();
        assert_eq!((img.mime.as_str(), img.b64.as_str()), ("image/webp", "BBBB"));
        assert!(o.text.contains("{\"factor\":2}"), "{}", o.text);
        // the image path missing is an error that shows the body
        let e = finish(&t, 200, br#"{"error":"busy"}"#, false, 1, &serde_json::json!({})).unwrap_err();
        assert!(e.contains("without an image") && e.contains("busy"), "{e}");

        let t = tool(serde_json::json!({ "name": "r", "url": "https://h", "result": { "text": "content" } }));
        let o = finish(&t, 200, br#"{"content":"hello","etag":"x"}"#, false, 1, &serde_json::json!({})).unwrap();
        assert_eq!(o.text, "hello");
        let o = finish(&t, 200, br#"{"other":1}"#, false, 1, &serde_json::json!({})).unwrap();
        assert_eq!(o.text, r#"{"other":1}"#);
        let e = finish(&t, 404, b"no such note", false, 1, &serde_json::json!({})).unwrap_err();
        assert!(e.contains("HTTP 404") && e.contains("no such note"), "{e}");
        let o = finish(&t, 200, b"partial", true, 99, &serde_json::json!({})).unwrap();
        assert!(o.text.ends_with("[response was cut off at 99 bytes]"), "{}", o.text);
    }

    #[test]
    fn sources_maps_extract_hits() {
        let t = tool(serde_json::json!({
            "name": "s", "url": "https://h/x",
            "sources": { "list": "results", "title": "meta.name", "url": "link" }
        }));
        let body = serde_json::json!({ "results": [
            { "meta": { "name": "First" }, "link": "https://a" },
            { "meta": {}, "link": "https://b" },
            { "meta": { "name": "" } }
        ]}).to_string();
        let o = finish(&t, 200, body.as_bytes(), false, 1, &serde_json::json!({})).unwrap();
        assert_eq!(o.sources, vec![("First".to_string(), "https://a".to_string()), ("https://b".to_string(), "https://b".to_string())]);
        let mut s = Vec::new();
        extract_sources(&t, "plain text", &mut s);
        assert!(s.is_empty());
    }

    /// Groups follow eyesoff-ai's rule exactly: own group, pictures, family
    /// name shared with a sibling, else the function name.
    #[test]
    fn groups_follow_the_family_rule() {
        let tools: Vec<HttpTool> = serde_json::from_value(serde_json::json!([
            { "name": "notes_read", "url": "https://h" },
            { "name": "notes_write", "url": "https://h" },
            { "name": "run_vm_command", "url": "https://h", "group": "virtual_machine" },
            { "name": "generate_image", "url": "https://h", "result": { "image": "d" } },
            { "name": "weather", "url": "https://h" }
        ])).unwrap();
        let g: Vec<String> = (0..tools.len()).map(|i| group_of(&tools, i)).collect();
        assert_eq!(g, vec!["notes", "notes", "virtual_machine", "images", "weather"]);
    }

    /// What tools/list says: schema, annotations from the method, and the
    /// `_meta` the eyesoff-ai client reads.
    #[test]
    fn describe_carries_the_meta_contract() {
        let t = tool(serde_json::json!({
            "name": "upscale_image", "url": "https://h/up", "method": "POST", "timeout_s": 120, "max_chars": 500,
            "format": "one line", "route": "when the user wants a bigger picture", "route_arg": "factor",
            "headers": { "x-user": "$user" },
            "body": { "image": "$image", "factor": "$factor" }, "result": { "image": "data.0.b64_json" }
        }));
        let d = describe(&t, "images");
        assert_eq!(d["name"], "upscale_image");
        assert_eq!(d["inputSchema"], serde_json::json!({ "type": "object", "properties": {} }));
        assert_eq!(d["annotations"]["readOnlyHint"], false);
        let m = &d["_meta"][crate::META_KEY];
        assert_eq!(m["group"], "images");
        assert_eq!(m["images"], true);
        assert_eq!(m["result"], "image");
        assert_eq!(m["timeout_s"], 120);
        assert_eq!(m["max_chars"], 500);
        assert_eq!(m["format"], "one line");
        assert_eq!(m["route_arg"], "factor");
        assert_eq!(m["user"], true);
        let plain = describe(&tool(serde_json::json!({ "name": "get", "url": "https://h" })), "get");
        assert_eq!(plain["annotations"]["readOnlyHint"], true);
        assert_eq!(plain["_meta"][crate::META_KEY], serde_json::json!({ "group": "get" }));
    }

    #[test]
    fn required_arguments_are_checked_by_name() {
        let t = tool(serde_json::json!({ "name": "a", "url": "https://h", "parameters": { "type": "object", "required": ["q", "n"] } }));
        assert_eq!(t.missing_required(&serde_json::json!({ "q": "x" })), vec!["n".to_string()]);
        assert!(t.missing_required(&serde_json::json!({ "q": "x", "n": 0 })).is_empty());
        assert_eq!(t.missing_required(&serde_json::json!({ "q": null })), vec!["q".to_string(), "n".to_string()]);
    }
}
