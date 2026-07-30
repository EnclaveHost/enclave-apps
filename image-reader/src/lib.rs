//! image-reader: a vision-language model that answers questions about images,
//! as a wasm component on Enclave's wasi-nn GPU interface.
//!
//! It ships NO weights. The model arrives as an attached Modelwrap volume
//! carrying a GGUF, a matching *mmproj*.gguf (the vision encoder and its
//! projector) and a tokenizer.json; the node preloads the graph at startup and
//! the guest load_by_name()s it. See fetch-model.sh for the reference volume.
//!
//! WHY THIS IS ITS OWN APP, and not a mode of the sibling llm-chat: seeing and
//! chatting have different lifecycles. A vision model is idle most of the time
//! and expensive while it runs, its VRAM share is sized by one dense KV window
//! rather than by a conversation backlog, and the thing an operator wants to do
//! with it is start it, stop it, resize it or restart it WITHOUT touching the
//! chat everyone is using. Two deployments, two funding rates, two lifecycles.
//! llm-chat then reaches this one over the fleet's network and folds the answer
//! into its own reply (its src/vision.rs), which means the chat model can be
//! the biggest thing the fleet holds while the eyes stay small and separate.
//!
//! Routes:
//!   GET  /            - the playground (self-contained HTML; drop an image in).
//!   GET  /ping        - liveness. Touches no wasi-nn, never authenticated.
//!   GET  /health      - what this deployment IS: attached volumes, which of
//!                       them carry a projector, the node's ggml tuning, the
//!                       VRAM budget and whether each model fits it.
//!                       ?probe=1 opens a real session and asks the HOST
//!                       whether it can see - the one check that answers
//!                       "will an image work here" without sending one.
//!   GET  /v1/models   - OpenAI-shaped catalog, with the enclave extras.
//!   POST /v1/vision   - the purpose-built shape: {image|images, question?,
//!                       context?, model?, max_tokens?, temperature?, system?}
//!                       -> {answer, image_tokens, timings, ...}. One call, one
//!                       question, prose back. This is what a program wants.
//!   POST /v1/chat/completions - OpenAI-compatible, including `stream: true`
//!                       and the three content-part spellings for images. This
//!                       is what an SDK wants.
//!   POST /ask         - the playground's SSE endpoint: {status} lines while
//!                       the session opens and the encoder runs, {delta} as the
//!                       answer streams, then {done, ...stats}.
//!
//! PRIVACY, precisely. The component imports wasi:nn and wasi:http's INCOMING
//! handler and nothing else: there is no outbound socket in this world, so an
//! image sent here cannot be forwarded anywhere, and a remote image URL is
//! refused because fetching one is not a thing this binary can do. Nothing is
//! written to disk (the model volume is read-only and there is no other mount),
//! so a picture exists in enclave RAM for the length of one request. The
//! attestation covers the component, so a caller can verify all of that rather
//! than believe it.
//!
//! WHAT IT DOES NOT DO, deliberately: no conversation storage (every request
//! carries its own turns), no speculative decoding (a picture does not advance
//! positions one-per-token, so the bookkeeping that makes drafting correct does
//! not apply), no image generation, no fetching. It looks, it answers, it
//! forgets.

#[allow(warnings)]
mod bindings;

mod config;
mod nn;
mod sampling;

use serde::Deserialize;
use tokenizers::Tokenizer;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;

use config::AppConfig;
use nn::{now_ms, GenParams, Turn};
use sampling::SampleParams;

static UI_HTML: &str = include_str!("ui.html");

/// Request-body ceiling. Generous because images arrive base64'd INSIDE the
/// JSON (~1.37x the file) and a request may legitimately carry several:
/// max_images * max_image_bytes * 1.37 has to fit, plus the words around them.
/// The per-image and per-request limits in nn::check_images are the real
/// policy; this is only the wall that stops a body from being read into guest
/// memory at all.
const MAX_BODY_BYTES: usize = 48 * 1024 * 1024;

// -------------------------------------------------------------- wire shapes --

/// One turn as it arrives. OpenAI's message content is either a string or an
/// array of typed parts; the array form is how images arrive, and there are
/// three spellings of it in the wild. All three are accepted, because the
/// alternative is a caller whose SDK "supports vision" getting a 400 for
/// reasons they cannot see:
///   {"type":"image_url","image_url":{"url":"data:image/png;base64,..."}}
///   {"type":"input_image","image_url":"data:..."}            (Responses API)
///   {"type":"image","source":{"type":"base64","data":"..."}} (Anthropic)
#[derive(Default)]
struct ChatMsg {
    role: String,
    content: String,
    images: Vec<Vec<u8>>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
struct ContentPart {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    image_url: Option<ImageRef>,
    #[serde(default)]
    source: Option<ImageSource>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ImageRef {
    Url(String),
    Obj { url: String },
}

#[derive(Deserialize)]
struct ImageSource {
    #[serde(default)]
    data: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

impl<'de> Deserialize<'de> for ChatMsg {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<ChatMsg, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            role: String,
            #[serde(default)]
            content: Option<RawContent>,
        }
        let w = Wire::deserialize(d)?;
        let mut msg = ChatMsg { role: w.role, ..Default::default() };
        match w.content {
            None => {}
            Some(RawContent::Text(t)) => msg.content = t,
            Some(RawContent::Parts(parts)) => {
                let mut text = String::new();
                for p in parts {
                    if let Some(t) = p.text {
                        if !text.is_empty() && !t.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(&t);
                    }
                    let src = match (&p.image_url, &p.source) {
                        (Some(ImageRef::Url(u)), _) => Some(u.clone()),
                        (Some(ImageRef::Obj { url }), _) => Some(url.clone()),
                        (None, Some(s)) => s.url.clone().or_else(|| s.data.clone()),
                        (None, None) => None,
                    };
                    if let Some(src) = src {
                        msg.images.push(decode_image_src(&src).map_err(serde::de::Error::custom)?);
                    }
                }
                msg.content = text;
            }
        }
        Ok(msg)
    }
}

/// The OpenAI-compatible request. Fields this app does not implement are
/// accepted and ignored, which is what lets an unmodified SDK talk to it.
#[derive(Deserialize)]
struct ChatReq {
    messages: Vec<ChatMsg>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    max_completion_tokens: Option<usize>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    stop: Option<serde_json::Value>,
    #[serde(default)]
    stream: bool,
}

/// The purpose-built shape: one picture, one question, prose back.
///
/// It exists next to /v1/chat/completions because the caller this app was built
/// for is a PROGRAM, and a program asking "what does this screenshot say" should
/// not have to assemble a messages array, wrap a data URI in two levels of
/// object, and then dig an answer out of choices[0].message.content. Every
/// field except the image is optional.
#[derive(Deserialize)]
struct VisionReq {
    /// one image, or several - both spellings, because both are natural
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    images: Vec<String>,
    /// what to answer. Absent = the config's default_question.
    #[serde(default)]
    question: Option<String>,
    /// aliases, so a caller's first guess works
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    q: Option<String>,
    /// Background the answer should be read against, but which is NOT itself a
    /// question: "this is a screenshot of our checkout page", or the spec the
    /// picture is supposed to match. It goes into the turn ahead of the
    /// question, which is where a VLM makes best use of it.
    ///
    /// This is the field that makes a model-authored query work: the caller's
    /// own model knows what matters about the picture and writes it here,
    /// instead of shipping the whole conversation to a second model.
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    top_k: Option<usize>,
}

impl VisionReq {
    fn question(&self) -> Option<&str> {
        [&self.question, &self.prompt, &self.q]
            .into_iter()
            .flatten()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
    }

    /// The turns to render: one user turn, pictures first, then context, then
    /// the question.
    fn turns(&self, cfg: &AppConfig) -> Result<Vec<Turn>, String> {
        let mut images = Vec::new();
        for src in self.image.iter().chain(self.images.iter()) {
            images.push(decode_image_src(src)?);
        }
        let mut text = String::new();
        if let Some(c) = self.context.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            text.push_str(c);
            text.push_str("\n\n");
        }
        text.push_str(self.question().unwrap_or(&cfg.default_question));
        Ok(vec![Turn { role: "user".into(), text, images }])
    }
}

// ------------------------------------------------------------------ images --

/// Turn one image reference into file bytes. Data URIs (and bare base64, which
/// is what the Anthropic shape sends) only: a remote URL is REFUSED rather than
/// fetched. This component has no outbound socket at all, so the refusal is
/// structural rather than a policy someone could switch off - and it is the
/// right policy anyway, because resolving a URL would tell a third-party host
/// what this enclave is looking at.
fn decode_image_src(src: &str) -> Result<Vec<u8>, String> {
    let s = src.trim();
    let b64 = if let Some(rest) = s.strip_prefix("data:") {
        let (meta, payload) =
            rest.split_once(',').ok_or("malformed data: URI (no comma before the payload)")?;
        if !meta.contains("base64") {
            return Err("only base64 data: URIs are supported for images".into());
        }
        payload
    } else if s.starts_with("http://") || s.starts_with("https://") {
        return Err("[url_image] image URLs are not fetched: send the image inline as a base64 \
                    data: URI. This component has no outbound network of any kind - fetching \
                    would tell a third-party host what you are looking at, which is the point \
                    of running the model in an enclave."
            .into());
    } else {
        s
    };
    let bytes = b64_decode(b64)?;
    match image_kind(&bytes) {
        Some(_) => Ok(bytes),
        None => Err("[image_undecodable] attachment is not a recognisable image (png, jpeg, \
                     webp, gif or bmp)"
            .into()),
    }
}

/// The image format, by magic bytes. Sniffing here rather than trusting the
/// data: URI's own mime type means a mislabelled upload fails with a sentence a
/// caller can act on, instead of inside the vision encoder.
fn image_kind(b: &[u8]) -> Option<&'static str> {
    if b.len() < 12 {
        return None;
    }
    if b.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("png");
    }
    if b.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpeg");
    }
    if b.starts_with(b"RIFF") && &b[8..12] == b"WEBP" {
        return Some("webp");
    }
    if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if b.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

/// Standard base64, tolerating whitespace and missing padding (both are common
/// in hand-assembled requests). No dependency for this: the crate carries a
/// tokenizer and serde and nothing else, and a decoder is twenty lines.
fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            b'\n' | b'\r' | b' ' | b'\t' => continue,
            _ => return Err("image payload is not valid base64".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.is_empty() {
        return Err("image payload is empty".into());
    }
    Ok(out)
}

// --------------------------------------------------------------- http plumbing --

fn read_body(req: &IncomingRequest) -> Result<Vec<u8>, String> {
    let body = req.consume().map_err(|_| "request has no body")?;
    let stream = body.stream().map_err(|_| "cannot read request body")?;
    let mut out = Vec::new();
    loop {
        match stream.blocking_read(64 * 1024) {
            Ok(chunk) => {
                out.extend_from_slice(&chunk);
                if out.len() > MAX_BODY_BYTES {
                    return Err(format!(
                        "request body exceeds {} MB - a base64 image is ~1.37x the file, so \
                         resize before sending",
                        MAX_BODY_BYTES / (1024 * 1024)
                    ));
                }
            }
            Err(StreamError::Closed) => break,
            Err(e) => return Err(format!("body read error: {e:?}")),
        }
    }
    Ok(out)
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

/// Machine-readable conditions the error messages tag inside themselves (as
/// "[code] "); json_err lifts the tag into `error.code` so a caller can branch
/// on it instead of pattern-matching prose. A caller that sees `no_vision` or
/// `vision_unsupported` knows the deployment is wrong; one that sees
/// `image_too_large` knows its own request is.
const ERR_CODES: &[&str] = &[
    "no_image", "too_many_images", "image_too_large", "image_undecodable", "url_image",
    "prompt_too_long", "no_vision", "vision_unsupported", "vision_unavailable", "image_too_wide",
    "sessions_busy", "model_not_loaded", "host_load_failed", "volume_not_attached",
];

fn strip_code(msg: &str) -> String {
    let mut m = msg.to_string();
    for c in ERR_CODES {
        m = m.replacen(&format!("[{c}] "), "", 1);
    }
    m
}

fn json_err(out: ResponseOutparam, status: u16, msg: &str) {
    let code = ERR_CODES.iter().copied().find(|c| msg.contains(&format!("[{c}] ")));
    let msg = strip_code(msg);
    let mut err = serde_json::json!({ "message": msg, "type": "invalid_request_error" });
    if let Some(c) = code {
        err["code"] = serde_json::json!(c);
    }
    respond_bytes(out, status, "application/json", serde_json::json!({ "error": err }).to_string().as_bytes());
}

/// Bearer check, with the SAME shape as the sibling apps: `api_key` gates the
/// `/v1/*` surface, and the playground's own routes stay open. That is a
/// deliberate reversal of how this app first shipped - it gated everything,
/// which meant setting a key locked an operator out of their own playground and
/// made a bare key prompt the first thing the page did.
///
/// So the trade is stated plainly rather than hidden: an unkeyed PUBLIC
/// deployment can be driven by anyone who reaches it, and inference is the
/// expensive kind of open door. The fleet's answer to that is a PRIVATE
/// deployment, which gates the data path at the supervisor where it belongs,
/// not an app-level password on a page that has nothing secret in it.
fn authorized(cfg: &AppConfig, req: &IncomingRequest) -> bool {
    let Some(key) = cfg.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) else {
        return true;
    };
    for v in req.headers().get(&"authorization".to_string()) {
        if let Ok(s) = String::from_utf8(v) {
            if let Some(tok) = s.strip_prefix("Bearer ") {
                if tok.trim() == key {
                    return true;
                }
            }
        }
    }
    false
}

/// The deployment-level config, for the fields that are policy rather than
/// per-model (the api_key). Reading it cannot fail in practice - the embedded
/// config parses in the test suite - but a hand-edited ENCLAVE_CONFIG can, and
/// then EVERY route should say so rather than one.
fn base_config(raw: &serde_json::Value) -> Result<AppConfig, String> {
    config::from_value(raw.clone()).map_err(|e| format!("configuration error: {e}"))
}

// ------------------------------------------------------------------ answering --

/// Everything the three answering routes share: pick the model, load the
/// tokenizer, vet the pictures, render the prompt, generate.
///
/// `emit`/`status` are the streaming hooks. A non-streaming caller passes an
/// `emit` that collects and a `status` that drops, and gets the same answer -
/// which is the point of doing it once here rather than three times.
#[allow(clippy::too_many_arguments)]
fn answer(
    raw: &serde_json::Value,
    model: Option<&str>,
    system: Option<&str>,
    turns: &[Turn],
    max_tokens: Option<usize>,
    sample_over: (Option<f32>, Option<f32>, Option<usize>),
    extra_stops: Vec<String>,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<(AppConfig, nn::GenStats), String> {
    let cfg = nn::resolve_model(raw, model)?;
    nn::check_images(&cfg, turns)?;
    let tok_bytes = nn::read_tokenizer(&cfg)?;
    let tok = Tokenizer::from_bytes(&tok_bytes).map_err(|e| format!("tokenizer: {e}"))?;
    let (prompt, stops) = nn::build_prompt(&cfg, &tok, system, turns)?;
    let (temperature, top_p, top_k) = sample_over;
    let mut stop_strings = stops;
    stop_strings.extend(extra_stops);
    let params = GenParams {
        max_new: max_tokens.unwrap_or(cfg.default_max_new).min(cfg.max_new_cap).max(1),
        sample: SampleParams {
            temperature: temperature.unwrap_or(cfg.temperature).clamp(0.0, 2.0),
            top_p: top_p.unwrap_or(cfg.top_p).clamp(0.05, 1.0),
            top_k: top_k.unwrap_or(cfg.top_k),
            rep_penalty: cfg.rep_penalty,
            rep_window: cfg.rep_window,
        },
        stop_strings,
        loop_reps: cfg.repeat_guard,
    };
    let stats = nn::generate(&cfg, &tok, &prompt, &params, emit, status)?;
    Ok((cfg, stats))
}

/// Stop strings a request asked for, on top of the template's own.
fn extra_stops(v: &Option<serde_json::Value>) -> Vec<String> {
    let mut stops = Vec::new();
    match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => stops.push(s.clone()),
        Some(serde_json::Value::Array(a)) => {
            for x in a.iter().take(4) {
                if let Some(s) = x.as_str() {
                    stops.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    stops
}

/// The per-answer numbers worth reporting, in one object. `image_tokens` is the
/// HOST's own figure for what the pictures cost - not the config's budget
/// estimate - because that is the number an operator sizing a share needs, and
/// the two are not close (a picture reported as 22 tokens can occupy 240
/// positions once M-RoPE has numbered its grid).
fn stats_json(cfg: &AppConfig, s: &nn::GenStats) -> serde_json::Value {
    let gen_s = (s.decode_ms as f64) / 1000.0;
    let tok_per_s = if gen_s > 0.0 { s.tokens as f64 / gen_s } else { 0.0 };
    serde_json::json!({
        "model": cfg.name,
        "images": s.images,
        "image_tokens": s.image_pos,
        "prompt_tokens": s.prompt_tokens,
        "tokens": s.tokens,
        "finish_reason": s.finish_reason,
        "load_ms": s.load_ms as u64,
        "prefill_ms": s.prefill_ms as u64,
        "decode_ms": s.decode_ms as u64,
        "ms": (s.load_ms + s.prefill_ms + s.decode_ms) as u64,
        "tok_per_s": (tok_per_s * 10.0).round() / 10.0,
    })
}

// --------------------------------------------------------------- POST /v1/vision --

fn handle_vision(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<VisionReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let vreq = match parsed {
        Ok(v) => v,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg0 = match nn::resolve_model(raw, vreq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let turns = match vreq.turns(&cfg0) {
        Ok(t) => t,
        Err(e) => return json_err(out, 400, &e),
    };
    let text = std::cell::RefCell::new(String::new());
    let collect = |d: &str| {
        text.borrow_mut().push_str(d);
        true
    };
    let quiet = |_: &str| true;
    match answer(
        raw,
        vreq.model.as_deref(),
        vreq.system.as_deref(),
        &turns,
        vreq.max_tokens,
        (vreq.temperature, vreq.top_p, vreq.top_k),
        Vec::new(),
        &collect,
        &quiet,
    ) {
        Ok((cfg, s)) => {
            let mut body = stats_json(&cfg, &s);
            body["answer"] = serde_json::json!(s.text.trim());
            body["question"] = serde_json::json!(vreq.question().unwrap_or(&cfg.default_question));
            respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
        }
        // 400 for anything the caller can fix, 503 for the deployment's own
        // state (busy sessions, a volume the node never loaded): a client
        // retrying is right in the second case and wrong in the first.
        Err(e) => {
            let status = if e.contains("[sessions_busy]")
                || e.contains("[model_not_loaded]")
                || e.contains("[host_load_failed]")
            {
                503
            } else if e.contains("[no_vision]") || e.contains("[vision_unsupported]") {
                501
            } else if e.contains('[') {
                400
            } else {
                500
            };
            json_err(out, status, &e)
        }
    }
}

// ------------------------------------------------- POST /v1/chat/completions --

fn completion_id() -> String {
    format!("chatcmpl-imgread{:x}", now_ms())
}

fn handle_completions(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let system = creq
        .messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone());
    let turns: Vec<Turn> = creq
        .messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| Turn { role: m.role.clone(), text: m.content.clone(), images: m.images.clone() })
        .collect();
    let max_tokens = creq.max_tokens.or(creq.max_completion_tokens);
    let sample = (creq.temperature, creq.top_p, creq.top_k);
    let stops = extra_stops(&creq.stop);

    if !creq.stream {
        let text = std::cell::RefCell::new(String::new());
        let collect = |d: &str| {
            text.borrow_mut().push_str(d);
            true
        };
        let quiet = |_: &str| true;
        return match answer(
            raw,
            creq.model.as_deref(),
            system.as_deref(),
            &turns,
            max_tokens,
            sample,
            stops,
            &collect,
            &quiet,
        ) {
            Ok((cfg, s)) => {
                let body = serde_json::json!({
                    "id": completion_id(),
                    "object": "chat.completion",
                    "created": now_ms() / 1000,
                    "model": cfg.name,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": s.text },
                        "finish_reason": if s.finish_reason == "length" { "length" } else { "stop" },
                    }],
                    "usage": {
                        "prompt_tokens": s.prompt_tokens + s.image_pos,
                        "completion_tokens": s.tokens,
                        "total_tokens": s.prompt_tokens + s.image_pos + s.tokens,
                    },
                    // the enclave extras: what the pictures really cost, and
                    // where the time went
                    "enclave": stats_json(&cfg, &s),
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
            }
            Err(e) => json_err(out, if e.contains("[sessions_busy]") { 503 } else { 400 }, &e),
        };
    }

    // -- streaming: OpenAI chunk objects over SSE
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    let send = |s: &str| -> bool {
        for chunk in s.as_bytes().chunks(4000) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return false;
            }
        }
        true
    };
    let id = completion_id();
    let model_name = nn::resolve_model(raw, creq.model.as_deref())
        .map(|c| c.name)
        .unwrap_or_else(|_| "unknown".into());
    let chunk = |delta: serde_json::Value, finish: Option<&str>| -> String {
        format!(
            "data: {}\n\n",
            serde_json::json!({
                "id": id, "object": "chat.completion.chunk",
                "created": now_ms() / 1000, "model": model_name,
                "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
            })
        )
    };
    let emit = |d: &str| send(&chunk(serde_json::json!({ "content": d }), None));
    // status lines are not part of the OpenAI stream shape, so they go out as
    // SSE COMMENTS: invisible to a client that does not care, and enough bytes
    // to keep a proxy from timing out the long silence while the encoder runs
    let status = |s: &str| send(&format!(": {}\n\n", s.replace('\n', " ")));
    let _ = send(&chunk(serde_json::json!({ "role": "assistant", "content": "" }), None));
    match answer(
        raw,
        creq.model.as_deref(),
        system.as_deref(),
        &turns,
        max_tokens,
        sample,
        stops,
        &emit,
        &status,
    ) {
        Ok((_, s)) => {
            let _ = send(&chunk(
                serde_json::json!({}),
                Some(if s.finish_reason == "length" { "length" } else { "stop" }),
            ));
        }
        Err(e) => {
            let _ = send(&format!(
                "data: {}\n\n",
                serde_json::json!({ "error": { "message": strip_code(&e) } })
            ));
        }
    }
    let _ = send("data: [DONE]\n\n");
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// ------------------------------------------------------------- POST /ask (UI) --

fn handle_ask(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<VisionReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let vreq = match parsed {
        Ok(v) => v,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg0 = match nn::resolve_model(raw, vreq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let turns = match vreq.turns(&cfg0) {
        Ok(t) => t,
        Err(e) => return json_err(out, 400, &e),
    };

    // The stream opens BEFORE the work starts, so the wait narrates itself:
    // opening a session on a cold node, then the encoder's pass over the
    // picture, are both long enough that silence reads as "hung".
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();
    let send = |v: serde_json::Value| -> bool {
        let msg = format!("data: {v}\n\n");
        for chunk in msg.as_bytes().chunks(4000) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return false;
            }
        }
        true
    };
    let emit = |d: &str| send(serde_json::json!({ "delta": d }));
    let status = |s: &str| send(serde_json::json!({ "status": s }));
    match answer(
        raw,
        vreq.model.as_deref(),
        vreq.system.as_deref(),
        &turns,
        vreq.max_tokens,
        (vreq.temperature, vreq.top_p, vreq.top_k),
        Vec::new(),
        &emit,
        &status,
    ) {
        Ok((cfg, s)) => {
            let mut done = stats_json(&cfg, &s);
            done["done"] = serde_json::json!(true);
            send(done);
        }
        Err(e) => {
            send(serde_json::json!({ "error": strip_code(&e) }));
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// ------------------------------------------------------------------- reads --

fn model_rows(raw: &serde_json::Value) -> (Vec<serde_json::Value>, Option<String>) {
    let entries = nn::available_models(raw);
    let unfit = nn::over_budget(&entries);
    let default = entries
        .iter()
        .find(|e| !unfit.contains_key(&e.volume) && e.mmproj.is_some())
        .or_else(|| entries.iter().find(|e| !unfit.contains_key(&e.volume)))
        .or_else(|| entries.first())
        .map(|e| e.cfg.name.clone());
    let rows = entries
        .iter()
        .map(|e| {
            let mut row = serde_json::json!({
                "id": e.cfg.name,
                "volume": e.volume,
                "weights_bytes": e.bytes,
                "mmproj_bytes": e.mmproj,
                // "can this model see at all", from the volume's contents. The
                // node's half of the answer needs a session - GET /health?probe=1
                "vision": e.mmproj.is_some(),
                "template": e.cfg.template,
                "max_prompt_tokens": e.cfg.max_prompt_tokens,
                "max_images": e.cfg.max_images,
                "image_tokens": e.cfg.image_tokens,
                "default": Some(&e.cfg.name) == default.as_ref(),
                "fits": !unfit.contains_key(&e.volume),
            });
            if let Some(why) = unfit.get(&e.volume) {
                row["why"] = serde_json::json!(why);
            }
            if e.mmproj.is_none() {
                row["why_no_vision"] = serde_json::json!(
                    "this volume carries no *mmproj*.gguf, so it holds a language model and \
                     nothing to see with - rewrap the volume with its projector included"
                );
            }
            row
        })
        .collect();
    (rows, default)
}

fn handle_models(raw: &serde_json::Value, out: ResponseOutparam) {
    let (rows, _) = model_rows(raw);
    let base = raw.get("name").and_then(|n| n.as_str()).unwrap_or("image-reader");
    let data: Vec<serde_json::Value> = if rows.is_empty() {
        // nothing servable attached: advertise the configured name so SDK flows
        // still see a model id (requests will explain what to attach)
        vec![serde_json::json!({ "id": base, "object": "model", "owned_by": "enclave-deployment" })]
    } else {
        rows.into_iter()
            .map(|r| {
                serde_json::json!({
                    "id": r["id"], "object": "model", "owned_by": "enclave-deployment",
                    "enclave": r,
                })
            })
            .collect()
    };
    let body = serde_json::json!({ "object": "list", "data": data });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

/// What this deployment IS, in one request - and with ?probe=1, what it can
/// actually DO. The probe is the check worth having: config and volume contents
/// can both say "vision" while the NODE's llama.cpp predates the projector
/// support, and the only way to know is to open a session and ask the host.
/// It costs one session open and no generation.
fn handle_health(raw: &serde_json::Value, query: &str, out: ResponseOutparam) {
    let (rows, default) = model_rows(raw);
    let node = serde_json::json!({
        "n_ctx": std::env::var("ENCLAVE_GGML_N_CTX").ok(),
        "kv_cache_type": std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").ok(),
        "kv_cache_type_v": std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").ok(),
        "max_sessions": std::env::var("ENCLAVE_GGML_MAX_SESSIONS").ok(),
        "pooled": std::env::var("ENCLAVE_GGML_POOLED").ok(),
        "image_max_tokens": std::env::var("ENCLAVE_GGML_IMAGE_MAX_TOKENS").ok(),
        "n_ubatch": std::env::var("ENCLAVE_GGML_N_UBATCH").ok(),
    });
    let mut body = serde_json::json!({
        "ok": true,
        "app": "image-reader",
        "version": env!("CARGO_PKG_VERSION"),
        "gpu": nn::gpu_present(),
        "vram_bytes": nn::vram_budget(),
        "attached": nn::attached_volumes(),
        "preloaded": nn::preloaded_graphs(),
        "models": rows,
        "default": default,
        "node": node,
        "t": now_ms(),
    });
    if query.split('&').any(|kv| kv == "probe=1" || kv == "probe=true") {
        let probe = match nn::resolve_model(raw, None) {
            Ok(cfg) => {
                let quiet = |_: &str| true;
                match nn::Session::open(&cfg, &quiet) {
                    Ok(mut s) => match s.caps() {
                        Ok(c) => serde_json::json!({
                            "ok": c.vision, "model": cfg.name, "host_vision": c.vision,
                            "note": if c.vision {
                                "the host reports a projector for this volume: images will work"
                            } else {
                                "the host reports NO projector: either the volume carries no \
                                 *mmproj*.gguf, or this node's llama.cpp toolchain predates \
                                 vision support. Images will be refused rather than ignored."
                            }
                        }),
                        // a host too old to answer caps at all is exactly the
                        // host that cannot see, and saying so is the point
                        Err(e) => serde_json::json!({
                            "ok": false, "model": cfg.name,
                            "note": format!(
                                "this node's host does not answer the capability verb, which \
                                 means its llama.cpp toolchain predates vision support ({})",
                                strip_code(&e)
                            )
                        }),
                    },
                    Err(e) => serde_json::json!({ "ok": false, "error": strip_code(&e) }),
                }
            }
            Err(e) => serde_json::json!({ "ok": false, "error": strip_code(&e) }),
        };
        body["probe"] = probe;
    }
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

struct Component;

impl Guest for Component {
    fn handle(req: IncomingRequest, out: ResponseOutparam) {
        let raw = match config::load_raw() {
            Ok(v) => v,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        let pq = req.path_with_query().unwrap_or_default();
        let path = pq.split('?').next().unwrap_or("/");
        let query = pq.split_once('?').map(|(_, q)| q).unwrap_or("");
        let method = req.method();

        // The playground's own surface, open like llm-chat's: the page, liveness,
        // the catalog the model picker reads, the health/probe an operator needs
        // when something is wrong, and the SSE endpoint the page posts to. A key
        // that locked these would lock the operator out of their own diagnostics.
        match (&method, path) {
            (Method::Get, "/") | (Method::Get, "") => {
                return respond_bytes(out, 200, "text/html; charset=utf-8", UI_HTML.as_bytes())
            }
            (Method::Get, "/ping") => {
                return respond_bytes(
                    out,
                    200,
                    "application/json",
                    format!("{{\"ok\":true,\"pong\":true,\"t\":{}}}", now_ms()).as_bytes(),
                )
            }
            (Method::Get, "/health") => return handle_health(&raw, query, out),
            (Method::Get, "/models") => return handle_models(&raw, out),
            (Method::Post, "/ask") => return handle_ask(&raw, req, out),
            _ => {}
        }

        let base = match base_config(&raw) {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &e),
        };
        if !authorized(&base, &req) {
            return json_err(
                out,
                401,
                "missing or invalid API key - send `Authorization: Bearer <key>`",
            );
        }
        match (method, path) {
            (Method::Get, "/v1/models") => handle_models(&raw, out),
            (Method::Post, "/v1/vision") | (Method::Post, "/vision") => {
                handle_vision(&raw, req, out)
            }
            (Method::Post, "/v1/chat/completions") => handle_completions(&raw, req, out),
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /ping, GET /health, GET /v1/models, \
                 POST /v1/vision, POST /v1/chat/completions, POST /ask",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 13, 0, 0,
    ];

    fn b64(b: &[u8]) -> String {
        const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            for i in 0..4 {
                if i <= c.len() {
                    s.push(T[(n >> (18 - 6 * i)) as usize & 63] as char);
                } else {
                    s.push('=');
                }
            }
        }
        s
    }

    #[test]
    fn base64_round_trips_through_a_data_uri() {
        let uri = format!("data:image/png;base64,{}", b64(PNG));
        assert_eq!(decode_image_src(&uri).unwrap(), PNG);
        // bare base64 (the Anthropic spelling) works too
        assert_eq!(decode_image_src(&b64(PNG)).unwrap(), PNG);
        // whitespace inside the payload is tolerated
        let split = format!("data:image/png;base64,{}\n{}", &b64(PNG)[..8], &b64(PNG)[8..]);
        assert_eq!(decode_image_src(&split).unwrap(), PNG);
    }

    #[test]
    fn a_remote_url_is_refused_with_the_reason() {
        let e = decode_image_src("https://example.com/cat.png").unwrap_err();
        assert!(e.starts_with("[url_image]"), "{e}");
        assert!(e.contains("no outbound network"));
    }

    #[test]
    fn a_mislabelled_upload_fails_here_not_in_the_encoder() {
        let e = decode_image_src(&format!("data:image/png;base64,{}", b64(b"not an image at all")))
            .unwrap_err();
        assert!(e.starts_with("[image_undecodable]"), "{e}");
    }

    #[test]
    fn magic_bytes_name_the_format() {
        assert_eq!(image_kind(PNG), Some("png"));
        assert_eq!(image_kind(&[0xff, 0xd8, 0xff, 0, 0, 0, 0, 0, 0, 0, 0, 0]), Some("jpeg"));
        assert_eq!(image_kind(b"RIFF____WEBP"), Some("webp"));
        assert_eq!(image_kind(b"GIF89a______"), Some("gif"));
        assert_eq!(image_kind(b"too short"), None);
    }

    #[test]
    fn the_openai_content_part_spellings_all_carry_an_image() {
        let uri = format!("data:image/png;base64,{}", b64(PNG));
        for parts in [
            serde_json::json!([{ "type": "text", "text": "what is this" },
                               { "type": "image_url", "image_url": { "url": uri } }]),
            serde_json::json!([{ "type": "input_image", "image_url": uri },
                               { "type": "text", "text": "what is this" }]),
            serde_json::json!([{ "type": "text", "text": "what is this" },
                               { "type": "image", "source": { "type": "base64", "data": b64(PNG) } }]),
        ] {
            let m: ChatMsg =
                serde_json::from_value(serde_json::json!({ "role": "user", "content": parts }))
                    .unwrap();
            assert_eq!(m.images.len(), 1);
            assert_eq!(m.images[0], PNG);
            assert_eq!(m.content, "what is this");
        }
        // and a plain string content still parses, with no images
        let m: ChatMsg =
            serde_json::from_value(serde_json::json!({ "role": "user", "content": "hi" })).unwrap();
        assert!(m.images.is_empty());
        assert_eq!(m.content, "hi");
    }

    #[test]
    fn the_vision_shape_accepts_every_question_alias() {
        let cfg = config::from_value(serde_json::json!({
            "name": "t", "n_layers": 1, "n_kv_heads": 1, "head_dim": 1, "vocab": 2, "eos": [0],
            "template": "chatml", "system_prompt": "s", "max_prompt_tokens": 10,
            "default_max_new": 1, "max_new_cap": 1, "model_volume": "v"
        }))
        .unwrap();
        let uri = format!("data:image/png;base64,{}", b64(PNG));
        for key in ["question", "prompt", "q"] {
            let v: VisionReq = serde_json::from_value(
                serde_json::json!({ "image": uri, key: "what does it say" }),
            )
            .unwrap();
            assert_eq!(v.question(), Some("what does it say"));
            let turns = v.turns(&cfg).unwrap();
            assert_eq!(turns.len(), 1);
            assert_eq!(turns[0].images.len(), 1);
            assert_eq!(turns[0].text, "what does it say");
        }
        // no question at all falls back to the config's default...
        let v: VisionReq = serde_json::from_value(serde_json::json!({ "image": uri })).unwrap();
        assert_eq!(v.question(), None);
        assert_eq!(v.turns(&cfg).unwrap()[0].text, cfg.default_question);
        // ...and context leads the question rather than replacing it
        let v: VisionReq = serde_json::from_value(serde_json::json!({
            "image": uri, "q": "does it match?", "context": "the spec says two buttons"
        }))
        .unwrap();
        assert_eq!(v.turns(&cfg).unwrap()[0].text, "the spec says two buttons\n\ndoes it match?");
    }

    #[test]
    fn several_images_arrive_in_the_order_given() {
        let cfg = config::from_value(serde_json::json!({
            "name": "t", "n_layers": 1, "n_kv_heads": 1, "head_dim": 1, "vocab": 2, "eos": [0],
            "template": "chatml", "system_prompt": "s", "max_prompt_tokens": 10,
            "default_max_new": 1, "max_new_cap": 1, "model_volume": "v"
        }))
        .unwrap();
        let jpeg: Vec<u8> = vec![0xff, 0xd8, 0xff, 1, 2, 3, 4, 5, 6, 7, 8, 9];
        let v: VisionReq = serde_json::from_value(serde_json::json!({
            "image": format!("data:image/png;base64,{}", b64(PNG)),
            "images": [format!("data:image/jpeg;base64,{}", b64(&jpeg))],
            "q": "compare"
        }))
        .unwrap();
        let t = v.turns(&cfg).unwrap();
        assert_eq!(t[0].images.len(), 2);
        assert_eq!(t[0].images[0], PNG);
        assert_eq!(t[0].images[1], jpeg);
    }

    #[test]
    fn error_codes_are_lifted_out_of_the_message() {
        assert_eq!(strip_code("[no_image] send a picture"), "send a picture");
        // every code the app emits is in the table, or json_err would leave the
        // marker in the human text
        for c in ["no_image", "too_many_images", "image_too_large", "prompt_too_long"] {
            assert!(ERR_CODES.contains(&c));
        }
    }

    #[test]
    fn a_key_is_required_only_when_one_is_configured() {
        let mut cfg = config::from_value(serde_json::json!({
            "name": "t", "n_layers": 1, "n_kv_heads": 1, "head_dim": 1, "vocab": 2, "eos": [0],
            "template": "chatml", "system_prompt": "s", "max_prompt_tokens": 10,
            "default_max_new": 1, "max_new_cap": 1, "model_volume": "v"
        }))
        .unwrap();
        assert!(cfg.api_key.is_none());
        // an api_key of "" or an unsubstituted "$NAME" placeholder must not
        // become a required credential nobody can produce
        cfg.api_key = Some("".into());
        // (authorized() needs a request, so the emptiness rule is asserted on
        // the same predicate it uses)
        assert!(cfg.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty()).is_none());
    }
}
