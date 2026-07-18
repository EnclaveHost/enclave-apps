use crate::bindings::exports::wasi::http::incoming_handler::Guest;
use crate::bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use crate::bindings::wasi::io::streams::StreamError;
use crate::bindings::wasi::nn::graph::ExecutionTarget;

use crate::config::{AppConfig, Catalog};
use crate::pipeline::{self, generate, to_png, GenRequest};
use serde::Deserialize;

static UI_HTML: &str = include_str!("ui.html");
const MAX_BODY_BYTES: usize = 64 * 1024;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ------------------------------------------------------------------ base64 --

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn b64encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

// -------------------------------------------------------------------- http --

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
        serde_json::json!({ "error": { "message": msg, "type": "invalid_request_error" } })
            .to_string()
            .as_bytes(),
    );
}

fn read_body(req: &IncomingRequest) -> Result<Vec<u8>, String> {
    let body = req.consume().map_err(|_| "request has no body")?;
    let stream = body.stream().map_err(|_| "cannot read request body")?;
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if out.len() > MAX_BODY_BYTES {
                    return Err("request body too large".into());
                }
            }
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("body read error: {e:?}")),
        }
    }
    Ok(out)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                if let Ok(v) = u8::from_str_radix(hex, 16) {
                    out.push(v);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_get(query: &str, key: &str) -> Option<String> {
    query
        .split('&')
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .map(percent_decode)
}

// ---------------------------------------------------------------- requests --

#[derive(Deserialize, Default)]
struct GenBody {
    /// which catalog entry serves this request; empty/absent = the default
    /// model (also the OpenAI request field, so /v1 clients get it for free)
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    steps: Option<usize>,
    #[serde(default)]
    seed: Option<u64>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    ancestral: Option<bool>,
    #[serde(default)]
    negative_prompt: Option<String>,
    #[serde(default)]
    cfg: Option<f32>,
    // OpenAI /v1/images/generations fields
    #[serde(default)]
    n: Option<usize>,
    #[serde(default)]
    size: Option<String>, // "512x512"
    #[serde(default)]
    response_format: Option<String>,
}

fn parse_target(cfg: &AppConfig, s: Option<&str>) -> Result<ExecutionTarget, String> {
    match s.unwrap_or(cfg.default_target.as_str()) {
        "cpu" => Ok(ExecutionTarget::Cpu),
        "gpu" => Ok(ExecutionTarget::Gpu),
        other => Err(format!("target must be gpu or cpu, got '{other}'")),
    }
}

/// Clamp + snap a requested dimension: sd.cpp wants multiples of 64.
fn snap_size(cfg: &AppConfig, v: Option<u32>) -> u32 {
    (v.unwrap_or(cfg.default_size).clamp(cfg.min_size, cfg.max_size) / 64) * 64
}

fn build_request(cfg: &AppConfig, b: &GenBody) -> Result<GenRequest, String> {
    let prompt = b.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err("prompt is required".into());
    }
    let (mut width, mut height) = (b.width, b.height);
    if let Some(sz) = &b.size {
        // OpenAI-style "WxH" overrides width/height when present
        let (w, h) = sz
            .split_once(['x', 'X'])
            .and_then(|(a, c)| Some((a.trim().parse().ok()?, c.trim().parse().ok()?)))
            .ok_or_else(|| format!("size must look like 512x512, got '{sz}'"))?;
        (width, height) = (Some(w), Some(h));
    }
    let req = GenRequest {
        prompt,
        negative_prompt: b.negative_prompt.clone().unwrap_or_default(),
        steps: b.steps.unwrap_or(cfg.default_steps).clamp(1, cfg.max_steps),
        seed: b.seed.unwrap_or_else(|| now_ms() as u64),
        width: snap_size(cfg, width),
        height: snap_size(cfg, height),
        cfg: b.cfg.unwrap_or(cfg.cfg_scale).clamp(0.0, 15.0),
        ancestral: b.ancestral.unwrap_or(true),
    };
    // placement is decided by the node's preload, but a bad target string is
    // still a client error worth rejecting loudly
    parse_target(cfg, b.target.as_deref())?;
    Ok(req)
}

fn timings_json(o: &pipeline::GenOutput, png_ms: u128) -> serde_json::Value {
    serde_json::json!({
        "load_ms": o.load_ms as u64,
        "gen_ms": o.gen_ms as u64,
        "png_ms": png_ms as u64,
    })
}

// --------------------------------------------------- POST /generate (SSE) --

fn handle_generate(cat: &Catalog, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<GenBody, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let body = match parsed {
        Ok(b) => b,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = match cat.get(body.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let greq = match build_request(cfg, &body) {
        Ok(r) => r,
        Err(e) => return json_err(out, 400, &e),
    };

    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let sse_body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = sse_body.write().unwrap();
    // ~4 KB SSE comment up front: TLS shims/proxies between the enclave and
    // the browser buffer small chunks, and the sdcpp path blocks in ONE long
    // compute() with nothing further to flush them - without padding the
    // early status events sit invisible in the buffer and clients see a
    // silent "starting..." for the whole generation.
    let pad = format!(":{}\n\n", " ".repeat(4000));
    for chunk in pad.as_bytes().chunks(4000) {
        if stream.blocking_write_and_flush(chunk).is_err() {
            break;
        }
    }
    let send = |v: serde_json::Value| -> bool {
        let msg = format!("data: {v}\n\n");
        for chunk in msg.as_bytes().chunks(4000) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return false;
            }
        }
        true
    };

    let mut status = |s: &str| send(serde_json::json!({ "status": s }));
    let result = generate(cfg, &greq, &mut status);
    match result {
        Ok(o) => {
            let t = now_ms();
            match to_png(&o.rgb, o.width, o.height) {
                Ok(png) => {
                    let png_ms = now_ms() - t;
                    send(serde_json::json!({
                        "done": true,
                        "image": b64encode(&png),
                        "model": cfg.name,
                        "width": o.width, "height": o.height,
                        "seed": greq.seed, "steps": greq.steps,
                        "timings": timings_json(&o, png_ms),
                    }));
                }
                Err(e) => {
                    send(serde_json::json!({ "error": e }));
                }
            }
        }
        Err(e) => {
            send(serde_json::json!({ "error": e }));
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(sse_body, None);
}

// ------------------------------------------------------------- GET /image --

fn handle_image(cat: &Catalog, query: &str, out: ResponseOutparam) {
    let body = GenBody {
        model: query_get(query, "model"),
        prompt: query_get(query, "prompt").unwrap_or_default(),
        steps: query_get(query, "steps").and_then(|s| s.parse().ok()),
        seed: query_get(query, "seed").and_then(|s| s.parse().ok()),
        width: query_get(query, "w").and_then(|s| s.parse().ok()),
        height: query_get(query, "h").and_then(|s| s.parse().ok()),
        target: query_get(query, "target"),
        ancestral: query_get(query, "ancestral").and_then(|s| s.parse().ok()),
        negative_prompt: query_get(query, "negative"),
        cfg: query_get(query, "cfg").and_then(|s| s.parse().ok()),
        ..Default::default()
    };
    let cfg = match cat.get(body.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let greq = match build_request(cfg, &body) {
        Ok(r) => r,
        Err(e) => return json_err(out, 400, &e),
    };
    let mut status = |_: &str| true; // nothing to stream on a plain GET
    let result = generate(cfg, &greq, &mut status);
    match result.and_then(|o| to_png(&o.rgb, o.width, o.height)) {
        Ok(png) => respond_bytes(out, 200, "image/png", &png),
        Err(e) => json_err(out, 500, &e),
    }
}

// ----------------------------------------- POST /v1/images/generations --

fn authorized(cfg: &AppConfig, req: &IncomingRequest) -> bool {
    let Some(key) = &cfg.api_key else { return true };
    for v in req.headers().get(&"authorization".to_string()) {
        if let Ok(s) = String::from_utf8(v) {
            if s.strip_prefix("Bearer ").map(str::trim) == Some(key) {
                return true;
            }
        }
    }
    false
}

fn handle_openai(cat: &Catalog, req: IncomingRequest, out: ResponseOutparam) {
    // api_key is an app-level setting; gate on the default model's before
    // touching the body (per-model keys would make 401s depend on parsing)
    if !authorized(cat.default_model(), &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let parsed: Result<GenBody, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let body = match parsed {
        Ok(b) => b,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = match cat.get(body.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    if matches!(body.response_format.as_deref(), Some("url")) {
        return json_err(
            out,
            400,
            "response_format 'url' is unsupported: an ephemeral enclave has nowhere durable to host files - use b64_json (the default here)",
        );
    }
    let n = body.n.unwrap_or(1).clamp(1, cfg.max_images);
    let base = match build_request(cfg, &body) {
        Ok(r) => r,
        Err(e) => return json_err(out, 400, &e),
    };
    let mut data = Vec::with_capacity(n);
    for i in 0..n {
        let mut greq = base.clone();
        greq.seed = base.seed.wrapping_add(i as u64);
        let mut status = |_: &str| true;
        match generate(cfg, &greq, &mut status)
            .and_then(|o| to_png(&o.rgb, o.width, o.height))
        {
            Ok(png) => data.push(serde_json::json!({
                "b64_json": b64encode(&png),
                "seed": greq.seed,
            })),
            Err(e) => return json_err(out, 500, &format!("image {}/{n}: {e}", i + 1)),
        }
    }
    let resp = serde_json::json!({ "created": (now_ms() / 1000) as u64, "data": data });
    respond_bytes(out, 200, "application/json", resp.to_string().as_bytes());
}

// ------------------------------------------------------------ warmup/info --

/// Weights bytes of a model's volume: the sum of its top-level model files
/// (the extensions the host's checkpoint picker considers). The ladder's
/// ordering key - residency is driven by weights on disk, NOT max_size,
/// which ranks capability, not VRAM.
fn volume_bytes(cfg: &AppConfig) -> u64 {
    let dir = std::path::Path::new(crate::config::MODELS_ROOT).join(&cfg.model_volume);
    let Ok(rd) = std::fs::read_dir(&dir) else { return 0 };
    rd.filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .map(|x| matches!(x, "gguf" | "safetensors" | "ckpt"))
                .unwrap_or(false)
        })
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

/// Warm one model: one step at `size` - warmth comes from resident weights
/// and warmed kernels, not pixels. An error IS the fit signal the ladder
/// consumes: a host preload skipped at boot (weights did not fit the share)
/// surfaces as the load_by_name error, a failed pipeline alloc as the
/// compute error - either way this model does not serve here.
fn warm_one(cfg: &AppConfig, size: u32) -> Result<(u128, pipeline::GenOutput), String> {
    let greq = GenRequest {
        prompt: "a lighthouse on a cliff at dawn".into(),
        negative_prompt: String::new(),
        steps: 1,
        seed: 0,
        width: size,
        height: size,
        cfg: cfg.cfg_scale,
        ancestral: true,
    };
    let t0 = now_ms();
    let mut status = |_: &str| true;
    generate(cfg, &greq, &mut status).map(|o| (now_ms() - t0, o))
}

/// GET /warmup - put weights and kernels in device memory BEFORE the first
/// real prompt.
///
/// `?model=` warms that ONE catalog entry (classic shape). BARE `/warmup` -
/// the manager's boot warmup and the playground's page load - is the
/// LADDER: every ATTACHED catalog model, smallest volume first, warmed one
/// at a time with a 1-step min_size generation, failures recorded and
/// skipped. Smallest-first is deliberate: residency within the share is
/// first-come-first-served, so the models most likely to fit are resident
/// (and guaranteed) before a bigger sibling claims - or fails to claim -
/// the rest; one published app serves whatever the deployment can hold and
/// the playground disables the models reported unfit. Unattached entries
/// are not probed - the UI already labels those "volume missing"; the
/// ladder answers the OTHER question: attached, but does it fit? 200 with
/// per-model results while at least one model warmed, 500 when none did.
///
/// Defaults to GPU ONLY: warmup exists to put weights in VRAM, and a failed
/// GPU should read as a failed warmup (pass ?target=cpu on dev boxes).
/// Warms at min_size unless ?size= says otherwise. Slow by design when
/// cold; repeat calls coalesce on the host's model cache.
fn handle_warmup(cat: &Catalog, query: &str, out: ResponseOutparam) {
    let model = query_get(query, "model");
    if let Some(want) = model.as_deref() {
        // single-model mode: the classic response shape
        let cfg = match cat.get(Some(want)) {
            Ok(c) => c,
            Err(e) => return json_err(out, 400, &e),
        };
        let target = match parse_target(cfg, query_get(query, "target").as_deref().or(Some("gpu"))) {
            Ok(t) => t,
            Err(e) => return json_err(out, 400, &e),
        };
        let size = snap_size(
            cfg,
            Some(
                query_get(query, "size")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(cfg.min_size),
            ),
        );
        return match warm_one(cfg, size) {
            Ok((total_ms, o)) => respond_bytes(
                out,
                200,
                "application/json",
                serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "target": if matches!(target, ExecutionTarget::Gpu) { "gpu" } else { "cpu" },
                    "size": size, "total_ms": total_ms as u64,
                    "timings": timings_json(&o, 0),
                })
                .to_string()
                .as_bytes(),
            ),
            Err(e) => json_err(out, 500, &e),
        };
    }

    // ladder mode: attached catalog models, smallest volume first
    if let Err(e) = parse_target(
        cat.default_model(),
        query_get(query, "target").as_deref().or(Some("gpu")),
    ) {
        return json_err(out, 400, &e);
    }
    let mut entries: Vec<(&AppConfig, u64)> = cat
        .models
        .iter()
        .filter(|m| m.volume_attached())
        .map(|m| (m, volume_bytes(m)))
        .collect();
    entries.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.model_volume.cmp(&b.0.model_volume)));
    if entries.is_empty() {
        // nothing attached: probing the default model yields the classic
        // volume-not-attached error that tells the operator what to attach
        let cfg = cat.default_model();
        return match warm_one(cfg, snap_size(cfg, Some(cfg.min_size))) {
            Ok(_) => json_err(out, 500, "no attached model volumes"),
            Err(e) => json_err(out, 500, &e),
        };
    }
    // The deployment's VRAM budget (ENCLAVE_VRAM_BYTES, set by the platform
    // from gpuShare x card VRAM): models it certainly cannot hold are
    // reported unfit WITHOUT probing - no point starting a multi-GB load
    // that must OOM. Cumulative smallest-first, mirroring the host's
    // preload order; weights-only on purpose (compute buffers come and go
    // per request), so only CERTAIN failures are skipped - borderline
    // models still get the honest probe. CPU-target warms skip the gate.
    let budget = std::env::var("ENCLAVE_VRAM_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|b| *b > 0)
        .filter(|_| query_get(query, "target").as_deref() != Some("cpu"));
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut claimed = 0u64;
    let mut ladder = Vec::with_capacity(entries.len());
    let mut default: Option<String> = None; // largest warmed = last ok in ascending order
    for (cfg, bytes) in &entries {
        if let Some(bud) = budget {
            if claimed + bytes > bud {
                ladder.push(serde_json::json!({
                    "model": cfg.name, "volume": cfg.model_volume, "bytes": bytes,
                    "ok": false, "skipped": true,
                    "error": format!(
                        "{:.1} GB of weights cannot fit the deployment's {:.1} GB VRAM \
                         budget ({:.1} GB claimed by smaller models) - redeploy with a \
                         larger GPU share to unlock this model",
                        gb(*bytes), gb(bud), gb(claimed)),
                }));
                continue;
            }
            claimed += bytes;
        }
        let size = snap_size(cfg, Some(cfg.min_size));
        match warm_one(cfg, size) {
            Ok((total_ms, o)) => {
                default = Some(cfg.name.clone());
                ladder.push(serde_json::json!({
                    "model": cfg.name, "volume": cfg.model_volume, "bytes": bytes,
                    "ok": true, "size": size, "total_ms": total_ms as u64,
                    "load_ms": o.load_ms as u64, "gen_ms": o.gen_ms as u64,
                }));
            }
            Err(e) => ladder.push(serde_json::json!({
                "model": cfg.name, "volume": cfg.model_volume, "bytes": bytes,
                "ok": false, "error": e,
            })),
        }
    }
    let ok = default.is_some();
    let body = serde_json::json!({ "ok": ok, "ladder": ladder, "default": default });
    respond_bytes(
        out,
        if ok { 200 } else { 500 },
        "application/json",
        body.to_string().as_bytes(),
    );
}

fn handle_info(cat: &Catalog, out: ResponseOutparam) {
    let attached = |m: &AppConfig| m.volume_attached();
    let models: Vec<serde_json::Value> = cat
        .models
        .iter()
        .map(|m| {
            serde_json::json!({
                "name": m.name,
                "volume": m.model_volume,
                "backend": "sdcpp",
                "attached": attached(m),
                "default_steps": m.default_steps, "max_steps": m.max_steps,
                "default_size": m.default_size,
                "min_size": m.min_size, "max_size": m.max_size,
                // sd.cpp wants /64 - the UI snaps its size list
                "size_step": 64,
            })
        })
        .collect();
    // top-level fields mirror the DEFAULT model - the pre-catalog shape,
    // kept so old clients/scripts don't break
    let cfg = cat.default_model();
    respond_bytes(
        out,
        200,
        "application/json",
        serde_json::json!({
            "name": cfg.name,
            "volume": cfg.model_volume,
            "volume_attached": attached(cfg),
            "attached": pipeline::attached_volumes(),
            "default_steps": cfg.default_steps, "max_steps": cfg.max_steps,
            "default_size": cfg.default_size,
            "min_size": cfg.min_size, "max_size": cfg.max_size,
            "default_target": cfg.default_target,
            "backend": "sdcpp",
            "models": models,
        })
        .to_string()
        .as_bytes(),
    );
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let cat = match crate::config::load() {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/");
        let query = pq.split_once('?').map(|(_, q)| q).unwrap_or("");
        match (req.method(), path) {
            (Method::Get, "/") | (Method::Get, "") => {
                respond_bytes(out, 200, "text/html; charset=utf-8", UI_HTML.as_bytes())
            }
            (Method::Get, "/ping") => respond_bytes(
                out,
                200,
                "application/json",
                format!("{{\"ok\":true,\"pong\":true,\"t\":{}}}", now_ms()).as_bytes(),
            ),
            (Method::Get, "/info") => handle_info(&cat, out),
            (Method::Get, "/warmup") => handle_warmup(&cat, query, out),
            (Method::Get, "/image") => handle_image(&cat, query, out),
            (Method::Post, "/generate") => handle_generate(&cat, req, out),
            (Method::Post, "/v1/images/generations") => handle_openai(&cat, req, out),
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /ping, GET /info, GET /warmup, GET /image, POST /generate, POST /v1/images/generations",
            ),
        }
    }
}

crate::bindings::export!(Component with_types_in crate::bindings);
