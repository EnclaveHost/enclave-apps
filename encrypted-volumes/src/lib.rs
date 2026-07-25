//! encrypted-volumes: the first-party sample for encrypted volumes (rclone
//! crypt over S3).
//!
//! A deployment's App Config names WHERE each volume's ciphertext lives
//! (`encVolumes`: S3 endpoint, bucket, path) - never a key. The platform
//! preopens an empty dir per volume at /enc/<name> (names in ENCLAVE_ENC) and
//! starts the app immediately. This app then serves the DECRYPTION UI: the
//! owner enters the rclone-crypt password (plus S3 credentials for a private
//! bucket) in the browser, over the deployment's in-enclave-terminated TLS,
//! and the app forwards it to the manager's loopback /encvol plane
//! (ENCLAVE_ENC_API, authenticated with ENCLAVE_ENC_TOKEN - which never
//! leaves the enclave and never reaches the browser). The manager pulls the
//! ciphertext from the bucket and decrypts it into the live preopen; the
//! files then appear here as ordinary std::fs reads.
//!
//! Routes:
//!   GET  /               - decryption UI + volume browser (self-contained HTML).
//!   GET  /api/status     - proxied GET  <ENCLAVE_ENC_API>            (adds the token; grafts each
//!                          volume's public credsEnvelope from ENCLAVE_CONFIG onto the response).
//!   POST /api/unlock     - proxied POST <ENCLAVE_ENC_API>/unlock     {name, password, salt?, accessKeyId?, secretAccessKey?, sessionToken?}
//!   POST /api/sync       - proxied POST <ENCLAVE_ENC_API>/sync       {name}  (push local edits back to the bucket)
//!   POST /api/lock       - proxied POST <ENCLAVE_ENC_API>/lock       {name}  (wipe plaintext, drop credentials)
//!   GET  /ls             - JSON: every volume in ENCLAVE_ENC, walked recursively.
//!   GET  /f/<vol>/<path> - raw file bytes from /enc/<vol>/<path>.
//!   GET  /ping           - liveness, touches no volume.
//!
//! What this demonstrates for app authors: your code needs NO crypto and no
//! S3 client - once unlocked, /enc/<name> is a normal directory. The bucket
//! and the operator's host only ever saw rclone-crypt ciphertext.
#[allow(warnings)]
mod bindings;

use std::io::Read;
use std::path::{Path, PathBuf};

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::outgoing_handler;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingRequest, OutgoingResponse,
    ResponseOutparam, Scheme,
};
use bindings::wasi::io::streams::StreamError;

static INDEX_HTML: &str = include_str!("index.html");
const MAX_LIST: usize = 10_000; // listing cap per volume (guard against huge trees)
const MAX_BODY: usize = 64 * 1024; // an /api request body (a password + creds) is small

/// Each volume's `credsEnvelope`, read from this deployment's own App Config
/// (ENCLAVE_CONFIG). The manager deliberately ignores the field - it is
/// wallet-sealed PUBLIC config data the UI needs at unlock time, so the
/// status proxy grafts it onto the manager's response by volume name.
fn creds_envelopes() -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Ok(cfg) = std::env::var("ENCLAVE_CONFIG") else { return out };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&cfg) else { return out };
    if let Some(vols) = v.get("encVolumes").and_then(|e| e.as_array()) {
        for e in vols {
            if let (Some(name), Some(env)) = (
                e.get("name").and_then(|x| x.as_str()),
                e.get("credsEnvelope").and_then(|x| x.as_str()),
            ) {
                out.insert(name.to_string(), env.to_string());
            }
        }
    }
    out
}

fn enc_names() -> Vec<String> {
    std::env::var("ENCLAVE_ENC")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, u64)>) {
    if out.len() >= MAX_LIST {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        let Ok(meta) = e.metadata() else { continue };
        if meta.is_dir() {
            walk(&p, root, out);
        } else if meta.is_file() {
            if let Ok(rel) = p.strip_prefix(root) {
                out.push((rel.to_string_lossy().replace('\\', "/"), meta.len()));
            }
            if out.len() >= MAX_LIST {
                return;
            }
        }
    }
}

/// /f/<vol>/<path> -> the on-disk path, refusing traversal and volumes not in
/// ENCLAVE_ENC. Plain segment filtering - no "..", no absolute jumps, no empties.
fn resolve(vol: &str, rel: &str) -> Option<PathBuf> {
    if !enc_names().iter().any(|n| n == vol) {
        return None;
    }
    let mut p = PathBuf::from("/enc").join(vol);
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return None;
        }
        p.push(seg);
    }
    Some(p)
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "txt" | "md" | "log" => "text/plain; charset=utf-8",
        "json" => "application/json",
        "html" | "htm" => "text/html; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn respond_bytes(out: ResponseOutparam, status: u16, ctype: &str, body_bytes: &[u8]) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
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

fn json_err(out: ResponseOutparam, status: u16, msg: &str) {
    respond_bytes(
        out,
        status,
        "application/json",
        serde_json::json!({ "error": { "message": msg } })
            .to_string()
            .as_bytes(),
    );
}

/// Drain the browser's request body (small: a password + credentials JSON).
fn read_request_body(req: &IncomingRequest) -> Vec<u8> {
    let mut out = Vec::new();
    let Ok(body) = req.consume() else { return out };
    let Ok(stream) = body.stream() else { return out };
    loop {
        match stream.blocking_read(16 * 1024) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if out.len() > MAX_BODY {
                    out.truncate(MAX_BODY);
                    break;
                }
            }
            Err(StreamError::Closed) => break,
            Err(_) => break,
        }
    }
    out
}

/// One call against the manager's /encvol plane. The bearer token is attached
/// HERE, server-side - the browser never sees ENCLAVE_ENC_TOKEN. Returns
/// (status, body) from the manager, verbatim.
fn api_call(method: Method, action: &str, body: Option<&[u8]>) -> Result<(u16, Vec<u8>), String> {
    let api = std::env::var("ENCLAVE_ENC_API")
        .map_err(|_| "ENCLAVE_ENC_API is not set: no encrypted volumes on this deployment")?;
    let token = std::env::var("ENCLAVE_ENC_TOKEN")
        .map_err(|_| "ENCLAVE_ENC_TOKEN is not set: no encrypted volumes on this deployment")?;
    let rest = api
        .strip_prefix("http://")
        .ok_or("ENCLAVE_ENC_API is not an http:// URL")?;
    let (authority, base_path) = match rest.split_once('/') {
        Some((a, p)) => (a.to_string(), format!("/{p}")),
        None => (rest.to_string(), String::new()),
    };
    let headers = Fields::new();
    let _ = headers.set(
        &"authorization".to_string(),
        &[format!("Bearer {token}").into_bytes()],
    );
    if let Some(b) = body {
        let _ = headers.set(
            &"content-type".to_string(),
            &[b"application/json".to_vec()],
        );
        // explicit content-length: without it wasi:http frames the body as
        // chunked, which the manager's stdlib HTTP server does not decode
        let _ = headers.set(
            &"content-length".to_string(),
            &[b.len().to_string().into_bytes()],
        );
    }
    let req = OutgoingRequest::new(headers);
    let _ = req.set_method(&method);
    let _ = req.set_scheme(Some(&Scheme::Http));
    let _ = req.set_authority(Some(&authority));
    let _ = req.set_path_with_query(Some(&format!("{base_path}{action}")));
    let out_body = req.body().map_err(|_| "request body unavailable")?;
    let fut = outgoing_handler::handle(req, None).map_err(|e| format!("dial manager: {e}"))?;
    if let Some(b) = body {
        let stream = out_body.write().map_err(|_| "request stream unavailable")?;
        for chunk in b.chunks(4000) {
            stream
                .blocking_write_and_flush(chunk)
                .map_err(|e| format!("send body: {e}"))?;
        }
        drop(stream);
    }
    OutgoingBody::finish(out_body, None).map_err(|e| format!("finish body: {e}"))?;
    fut.subscribe().block();
    let resp = fut
        .get()
        .ok_or("manager response missing")?
        .map_err(|_| "manager response taken twice")?
        .map_err(|e| format!("manager request failed: {e}"))?;
    let status = resp.status();
    let mut out = Vec::new();
    if let Ok(body) = resp.consume() {
        if let Ok(stream) = body.stream() {
            loop {
                match stream.blocking_read(16 * 1024) {
                    Ok(chunk) => out.extend_from_slice(&chunk),
                    Err(StreamError::Closed) => break,
                    Err(_) => break,
                }
            }
        }
    }
    Ok((status, out))
}

/// GET /api/status | POST /api/{unlock,sync,lock}: forward to the manager and
/// relay its JSON + status code straight back to the browser.
fn handle_api(out: ResponseOutparam, method: Method, action: &str, body: Option<&[u8]>) {
    match api_call(method, action, body) {
        Ok((status, bytes)) => respond_bytes(out, status, "application/json", &bytes),
        Err(msg) => json_err(out, 503, &msg),
    }
}

/// Stream a file straight from the volume into the response body - the file
/// may be bigger than guest memory wants to hold.
fn serve_file(out: ResponseOutparam, path: &Path, ctype: &str) {
    let Ok(mut f) = std::fs::File::open(path) else {
        return json_err(out, 404, "no such file in this volume");
    };
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let _ = resp.set_status_code(200);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    let mut buf = [0u8; 4000];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stream.blocking_write_and_flush(&buf[..n]).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

fn handle_ls(out: ResponseOutparam) {
    let mut vols = serde_json::Map::new();
    for name in enc_names() {
        let root = PathBuf::from("/enc").join(&name);
        let mut files = Vec::new();
        walk(&root, &root, &mut files);
        vols.insert(
            name,
            serde_json::json!({
                "files": files.iter().map(|(p, s)| serde_json::json!({ "path": p, "size": s })).collect::<Vec<_>>(),
                "truncated": files.len() >= MAX_LIST,
            }),
        );
    }
    let body = serde_json::json!({ "volumes": vols });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/");
        match (req.method(), path) {
            (Method::Get, "/") | (Method::Get, "") => {
                respond_bytes(out, 200, "text/html; charset=utf-8", INDEX_HTML.as_bytes())
            }
            (Method::Get, "/ping") => {
                respond_bytes(out, 200, "application/json", b"{\"ok\":true,\"pong\":true}")
            }
            (Method::Get, "/api/status") => match api_call(Method::Get, "", None) {
                Ok((status, bytes)) => {
                    let envs = creds_envelopes();
                    let bytes = if status == 200 && !envs.is_empty() {
                        match serde_json::from_slice::<serde_json::Value>(&bytes) {
                            Ok(mut v) => {
                                if let Some(vols) =
                                    v.get_mut("volumes").and_then(|x| x.as_array_mut())
                                {
                                    for vol in vols {
                                        let Some(obj) = vol.as_object_mut() else { continue };
                                        let Some(name) =
                                            obj.get("name").and_then(|x| x.as_str()).map(str::to_string)
                                        else {
                                            continue;
                                        };
                                        if let Some(env) = envs.get(&name) {
                                            obj.insert(
                                                "credsEnvelope".into(),
                                                serde_json::Value::String(env.clone()),
                                            );
                                        }
                                    }
                                }
                                v.to_string().into_bytes()
                            }
                            Err(_) => bytes,
                        }
                    } else {
                        bytes
                    };
                    respond_bytes(out, status, "application/json", &bytes)
                }
                Err(msg) => json_err(out, 503, &msg),
            },
            (Method::Post, "/api/unlock") | (Method::Post, "/api/sync") | (Method::Post, "/api/lock") => {
                let action = format!("/{}", &path["/api/".len()..]);
                let body = read_request_body(&req);
                handle_api(out, Method::Post, &action, Some(&body))
            }
            (Method::Get, "/ls") => handle_ls(out),
            (Method::Get, p) if p.starts_with("/f/") => {
                let rest = &p[3..];
                let Some((vol, rel)) = rest.split_once('/') else {
                    return json_err(out, 400, "use /f/<volume>/<path>");
                };
                match resolve(vol, rel) {
                    Some(disk) => serve_file(out, &disk, content_type(rel)),
                    None => json_err(out, 404, "unknown volume or bad path"),
                }
            }
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /api/status, POST /api/{unlock,sync,lock}, GET /ls, GET /f/<volume>/<path>, GET /ping",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);
