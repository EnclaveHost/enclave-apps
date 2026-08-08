//! speech-generator: text to speech on Enclave's wasi-nn GPU interface, as a
//! wasm component with a built-in web UI.
//!
//! MODEL: Maya1 (maya-research/maya1, Apache-2.0) - the strongest open-weights
//! speech model that is also llama.cpp-shaped: a Llama-3.2-3B that predicts
//! SNAC audio-codec tokens, with the voice described in natural language
//! ("Male voice in his 40s, deep and calm...") and 20+ inline emotion tags
//! (<laugh>, <sigh>, <whisper>, ...). The host runs it like any other GGUF
//! volume; nothing platform-side knows this app makes sound.
//!
//! THE SPLIT: the 3B transformer runs on the GPU share behind wasi-nn's
//! `tokens` verb, exactly like eyesoff-ai's models. The guest samples the audio
//! tokens itself (the host returns dense logits, so the sampler can enforce
//! the 7-slot frame structure by construction - sampling.rs), unpacks frames
//! into SNAC codebook streams (maya.rs), and runs the SNAC 24 kHz decoder -
//! 13M parameters of pure-Rust convolutions - on the wasm CPU (snac.rs),
//! streaming 16-bit WAV as it goes. No audio verb exists host-side; this app
//! is the proof it is not needed for speech OUT.
//!
//! Routes:
//!   GET  /            - the playground.
//!   GET  /ping        - liveness. Touches no wasi-nn, never authenticated.
//!   GET  /health      - volumes, VRAM budget, node tuning; ?probe=1 opens a
//!                       real session AND parses the volume's SNAC decoder -
//!                       the two halves of "will this deployment speak".
//!   GET  /voices      - the preset table (name -> description) and default.
//!   GET  /models      - the catalog.
//!   GET|POST /speak   - the direct shape: text/voice/description in a query
//!                       string or JSON body, a WAV stream back. GET makes
//!                       curl -o out.wav ".../speak?text=hello" work.
//!   GET  /v1/models   - OpenAI-shaped catalog.               (Bearer-gated)
//!   POST /v1/audio/speech - OpenAI-compatible: {model, input, voice,
//!                       response_format: wav|pcm, instructions} -> audio.
//!                       `instructions` maps to the voice description, which
//!                       is exactly what that field means on the OpenAI TTS
//!                       endpoint.                            (Bearer-gated)
//!
//! PRIVACY: the component imports wasi:nn and wasi:http's INCOMING handler and
//! nothing else - no outbound socket exists in this world, so text sent here
//! cannot be forwarded anywhere and the audio exists only in the response
//! stream. Nothing is written to disk (the model volume is read-only). The
//! attestation covers the component; a caller can verify rather than believe.
//!
//! The response is a STREAM whose first bytes (the WAV header) go out before
//! the session opens, and whose keepalive during waits is a trickle of
//! silence samples - the fleet's gateway cuts a response ~180 s after the
//! last byte, and a header-then-silence stream never triggers that.

#[allow(warnings)]
mod bindings;

mod config;
mod maya;
mod nn;
mod sampling;
mod snac;
mod wav;

use serde::Deserialize;
use tokenizers::Tokenizer;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;

use config::AppConfig;
use nn::now_ms;
use sampling::SampleParams;

static UI_HTML: &str = include_str!("ui.html");

const MAX_BODY_BYTES: usize = 1024 * 1024;

// -------------------------------------------------------------- wire shapes --

/// One speech request, from either surface. /speak accepts exactly this as
/// JSON or as query parameters; /v1/audio/speech maps OpenAI's fields onto it
/// (`input`, `voice`, `instructions` -> description).
#[derive(Deserialize, Default)]
struct SpeakReq {
    /// the text to speak; `input` is the OpenAI spelling, `text` the natural one
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// preset name, or a free-form voice description (see AppConfig::resolve_voice)
    #[serde(default)]
    voice: Option<String>,
    /// explicit voice description; wins over `voice`
    #[serde(default)]
    description: Option<String>,
    /// OpenAI's name for "how should it sound" on the TTS endpoint
    #[serde(default)]
    instructions: Option<String>,
    #[serde(default)]
    model: Option<String>,
    /// "wav" (default) or "pcm" (raw s16le mono 24 kHz, which is also exactly
    /// OpenAI's pcm format)
    #[serde(default)]
    response_format: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    /// determinism for the sampler and the codec's noise; absent = clock
    #[serde(default)]
    seed: Option<u64>,
    /// accepted for OpenAI compatibility; time-stretching is not implemented,
    /// and silently mangling the audio would be worse than saying so
    #[serde(default)]
    speed: Option<f32>,
}

impl SpeakReq {
    fn text(&self) -> Option<&str> {
        [&self.input, &self.text]
            .into_iter()
            .flatten()
            .map(|s| s.as_str())
            .find(|s| !s.trim().is_empty())
    }

    fn description(&self) -> Option<&str> {
        [&self.description, &self.instructions]
            .into_iter()
            .flatten()
            .map(|s| s.as_str())
            .find(|s| !s.trim().is_empty())
    }

    fn wire_format(&self) -> &str {
        [&self.response_format, &self.format]
            .into_iter()
            .flatten()
            .map(|s| s.trim())
            .find(|s| !s.is_empty())
            .unwrap_or("wav")
    }

    fn from_query(query: &str) -> SpeakReq {
        let mut r = SpeakReq::default();
        for kv in query.split('&') {
            let (k, v) = match kv.split_once('=') {
                Some((k, v)) => (k, percent_decode(v)),
                None => continue,
            };
            match k {
                "text" | "input" => r.input = Some(v),
                "voice" => r.voice = Some(v),
                "description" | "instructions" => r.description = Some(v),
                "model" => r.model = Some(v),
                "format" | "response_format" => r.response_format = Some(v),
                "temperature" => r.temperature = v.parse().ok(),
                "top_p" => r.top_p = v.parse().ok(),
                "seed" => r.seed = v.parse().ok(),
                _ => {}
            }
        }
        r
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                };
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push(h << 4 | l);
                        i += 3;
                    }
                    _ => {
                        out.push(b[i]);
                        i += 1;
                    }
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
                    return Err("request body exceeds 1 MB - this endpoint takes text".into());
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

/// Machine-readable conditions tagged "[code] " inside error messages, lifted
/// into `error.code` so a caller can branch without parsing prose.
const ERR_CODES: &[&str] = &[
    "no_text", "text_too_long", "bad_format", "sessions_busy", "model_not_loaded",
    "host_load_failed", "volume_not_attached", "no_snac",
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
    respond_bytes(
        out,
        status,
        "application/json",
        serde_json::json!({ "error": err }).to_string().as_bytes(),
    );
}

fn err_status(e: &str) -> u16 {
    if e.contains("[sessions_busy]")
        || e.contains("[model_not_loaded]")
        || e.contains("[host_load_failed]")
    {
        503
    } else if e.contains('[') {
        400
    } else {
        500
    }
}

/// Bearer check, same posture as the sibling apps: `api_key` gates the /v1/*
/// surface, the playground's own routes stay open, and a PRIVATE deployment is
/// the fleet's real answer for locking the data path.
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

fn base_config(raw: &serde_json::Value) -> Result<AppConfig, String> {
    config::from_value(raw.clone()).map_err(|e| format!("configuration error: {e}"))
}

// ---------------------------------------------------------------- speaking --

/// Everything a speech request needs, prepared BEFORE the response stream
/// opens - every error here still has a JSON status code to ride out on.
struct Prepared {
    cfg: AppConfig,
    tok: Tokenizer,
    dec: snac::Decoder,
    params: nn::SpeakParams,
    pcm_only: bool,
}

fn prepare(raw: &serde_json::Value, r: &SpeakReq) -> Result<Prepared, String> {
    let pcm_only = match r.wire_format() {
        "wav" => false,
        "pcm" => true,
        other => {
            return Err(format!(
                "[bad_format] response_format '{other}' is not available: this component \
                 encodes wav (default) and pcm (raw s16le mono 24 kHz). There is no mp3/opus \
                 encoder in this world - WAV is what every player and every browser accepts."
            ))
        }
    };
    if r.speed.map_or(false, |s| (s - 1.0).abs() > 0.01) {
        return Err("[bad_format] `speed` is not implemented: this endpoint returns the \
                    model's own pacing (ask the voice description for a faster or slower \
                    speaker instead)"
            .into());
    }
    let cfg = nn::resolve_model(raw, r.model.as_deref())?;
    let text = maya::clean_text(r.text().unwrap_or_default());
    if text.is_empty() {
        return Err("[no_text] nothing to speak - put the text in `input` (or `text`)".into());
    }
    if text.chars().count() > cfg.max_text_chars {
        return Err(format!(
            "[text_too_long] this request is {} characters; the limit is {} - split it into \
             several requests (each ~{} chars is one generation episode anyway)",
            text.chars().count(),
            cfg.max_text_chars,
            cfg.chunk_max_chars
        ));
    }
    let chunks = maya::chunk_text(&text, cfg.chunk_max_chars);
    if chunks.is_empty() {
        return Err("[no_text] nothing to speak after cleaning".into());
    }
    let tok_bytes = nn::read_tokenizer(&cfg)?;
    let tok = Tokenizer::from_bytes(&tok_bytes).map_err(|e| format!("tokenizer: {e}"))?;
    let snac_bytes = nn::read_snac(&cfg).map_err(|e| format!("[no_snac] {e}"))?;
    let dec = snac::Decoder::from_bytes(&snac_bytes)?;
    let desc = cfg.resolve_voice(r.voice.as_deref(), r.description());
    let params = nn::SpeakParams {
        desc,
        chunks,
        sample: SampleParams {
            temperature: r.temperature.unwrap_or(cfg.temperature).clamp(0.0, 1.5),
            top_p: r.top_p.unwrap_or(cfg.top_p).clamp(0.05, 1.0),
            rep_penalty: cfg.rep_penalty,
            rep_window: cfg.rep_window,
        },
        max_new_per_chunk: cfg.max_new_tokens.clamp(maya::FRAME_TOKENS, 8192),
        min_frames: cfg.min_frames.max(1),
        trim_warmup: cfg.trim_warmup_samples,
        seed: r.seed.unwrap_or_else(|| now_ms() as u64),
    };
    Ok(Prepared { cfg, tok, dec, params, pcm_only })
}

fn handle_speak(raw: &serde_json::Value, r: SpeakReq, out: ResponseOutparam) {
    let prep = match prepare(raw, &r) {
        Ok(p) => p,
        Err(e) => return json_err(out, err_status(&e), &e),
    };

    let headers = Fields::new();
    let ctype: &[u8] = if prep.pcm_only { b"audio/pcm" } else { b"audio/wav" };
    let _ = headers.set(&"content-type".to_string(), &[ctype.to_vec()]);
    let _ = headers.set(&"cache-control".to_string(), &[b"no-store".to_vec()]);
    let _ = headers.set(&"x-model".to_string(), &[prep.cfg.name.as_bytes().to_vec()]);
    let resp = OutgoingResponse::new(headers);
    let body = resp.body().unwrap();
    ResponseOutparam::set(out, Ok(resp));
    let stream = body.write().unwrap();

    let write_all = |bytes: &[u8]| -> bool {
        for chunk in bytes.chunks(4000) {
            if stream.blocking_write_and_flush(chunk).is_err() {
                return false;
            }
        }
        true
    };
    // the header is the first keepalive byte; from here on every error is a
    // truncated stream rather than a status code, which is what streaming buys
    if !prep.pcm_only && !write_all(&wav::header(wav::STREAMING_SIZE)) {
        return;
    }
    let emit = |samples: &[f32]| write_all(&wav::pcm16_bytes(samples));
    // during session queues and long prefills the stream stays warm with a
    // millisecond of silence per status tick - inaudible, and enough bytes
    // that neither the gateway nor the listener's player gives up
    let status = |_msg: &str| write_all(&[0u8; 48]);
    let _ = nn::generate_speech(&prep.cfg, &prep.tok, &prep.dec, &prep.params, &emit, &status);
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// ------------------------------------------------------------------- reads --

fn model_rows(raw: &serde_json::Value) -> (Vec<serde_json::Value>, Option<String>) {
    let entries = nn::available_models(raw);
    let unfit = nn::over_budget(&entries);
    let default = entries
        .iter()
        .find(|e| !unfit.contains_key(&e.volume) && e.snac)
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
                "snac": e.snac,
                "voices": e.cfg.voices.keys().collect::<Vec<_>>(),
                "default_voice": e.cfg.default_voice,
                "max_text_chars": e.cfg.max_text_chars,
                "sample_rate": snac::SAMPLE_RATE,
                "default": Some(&e.cfg.name) == default.as_ref(),
                "fits": !unfit.contains_key(&e.volume),
            });
            if let Some(why) = unfit.get(&e.volume) {
                row["why"] = serde_json::json!(why);
            }
            if !e.snac {
                row["why_mute"] = serde_json::json!(
                    "this volume carries no snac_decoder.bin, so its audio tokens cannot be \
                     decoded - rebuild the volume with fetch-model.sh (tools/export_snac.py \
                     produces the file)"
                );
            }
            row
        })
        .collect();
    (rows, default)
}

fn handle_models(raw: &serde_json::Value, out: ResponseOutparam) {
    let (rows, _) = model_rows(raw);
    let base = raw.get("name").and_then(|n| n.as_str()).unwrap_or("speech-generator");
    let data: Vec<serde_json::Value> = if rows.is_empty() {
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

fn handle_voices(raw: &serde_json::Value, out: ResponseOutparam) {
    match base_config(raw) {
        Ok(cfg) => {
            let body = serde_json::json!({
                "voices": cfg.voices,
                "default": cfg.default_voice,
                "note": "a request's `voice` may name one of these OR carry its own \
                         natural-language description; `description`/`instructions` \
                         overrides either. Inline emotion tags like <laugh>, <sigh>, \
                         <whisper>, <gasp>, <angry>, <giggle>, <chuckle>, <cry>, \
                         <excited>, <sing> go in the text itself.",
            });
            respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
        }
        Err(e) => json_err(out, 500, &e),
    }
}

/// What this deployment IS; ?probe=1 adds what it can DO - a real session open
/// (the GPU half) and a real parse of the volume's SNAC decoder (the CPU
/// half). Between them that is "will this deployment speak", without
/// generating a syllable.
fn handle_health(raw: &serde_json::Value, query: &str, out: ResponseOutparam) {
    let (rows, default) = model_rows(raw);
    let node = serde_json::json!({
        "n_ctx": std::env::var("ENCLAVE_GGML_N_CTX").ok(),
        "kv_cache_type": std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").ok(),
        "kv_cache_type_v": std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").ok(),
        "max_sessions": std::env::var("ENCLAVE_GGML_MAX_SESSIONS").ok(),
        "pooled": std::env::var("ENCLAVE_GGML_POOLED").ok(),
    });
    let mut body = serde_json::json!({
        "ok": true,
        "app": "speech-generator",
        "version": env!("CARGO_PKG_VERSION"),
        "gpu": nn::gpu_present(),
        "vram_bytes": nn::vram_budget(),
        "attached": nn::attached_volumes(),
        "preloaded": nn::preloaded_graphs(),
        "models": rows,
        "default": default,
        "sample_rate": snac::SAMPLE_RATE,
        "node": node,
        "t": now_ms(),
    });
    if query.split('&').any(|kv| kv == "probe=1" || kv == "probe=true") {
        let probe = match nn::resolve_model(raw, None) {
            Ok(cfg) => {
                let snac_probe = match nn::read_snac(&cfg)
                    .and_then(|b| snac::Decoder::from_bytes(&b).map(|_| b.len()))
                {
                    Ok(bytes) => serde_json::json!({ "ok": true, "bytes": bytes }),
                    Err(e) => serde_json::json!({ "ok": false, "error": strip_code(&e) }),
                };
                let quiet = |_: &str| true;
                let t0 = now_ms();
                let session_probe = match nn::Session::open(&cfg, &quiet) {
                    Ok(_s) => serde_json::json!({ "ok": true, "open_ms": (now_ms() - t0) as u64 }),
                    Err(e) => serde_json::json!({ "ok": false, "error": strip_code(&e) }),
                };
                serde_json::json!({
                    "ok": snac_probe["ok"] == true && session_probe["ok"] == true,
                    "model": cfg.name,
                    "session": session_probe,
                    "snac_decoder": snac_probe,
                })
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

        // the playground's own surface stays open (see image-reader for the
        // posture and its reasoning); /speak is the playground's engine the
        // way /ask is image-reader's
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
            (Method::Get, "/voices") => return handle_voices(&raw, out),
            (Method::Get, "/models") => return handle_models(&raw, out),
            (Method::Get, "/speak") | (Method::Get, "/speak.wav") => {
                return handle_speak(&raw, SpeakReq::from_query(query), out)
            }
            (Method::Post, "/speak") => {
                let parsed: Result<SpeakReq, String> = read_body(&req).and_then(|b| {
                    serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}"))
                });
                return match parsed {
                    Ok(r) => handle_speak(&raw, r, out),
                    Err(e) => json_err(out, 400, &e),
                };
            }
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
            (Method::Post, "/v1/audio/speech") | (Method::Post, "/audio/speech") => {
                let parsed: Result<SpeakReq, String> = read_body(&req).and_then(|b| {
                    serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}"))
                });
                match parsed {
                    Ok(r) => handle_speak(&raw, r, out),
                    Err(e) => json_err(out, 400, &e),
                }
            }
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /ping, GET /health, GET /voices, GET /models, \
                 GET|POST /speak, GET /v1/models, POST /v1/audio/speech",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing_covers_the_curl_case() {
        let r = SpeakReq::from_query("text=Hello%20there%2C%20world&voice=sage&seed=7");
        assert_eq!(r.text(), Some("Hello there, world"));
        assert_eq!(r.voice.as_deref(), Some("sage"));
        assert_eq!(r.seed, Some(7));
        // + is a space, and unknown keys are ignored
        let r = SpeakReq::from_query("input=a+b&x=1&format=pcm");
        assert_eq!(r.text(), Some("a b"));
        assert_eq!(r.wire_format(), "pcm");
    }

    #[test]
    fn percent_decoding_is_safe_on_malformed_input() {
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
        assert_eq!(percent_decode("%41"), "A");
        assert_eq!(percent_decode(""), "");
    }

    #[test]
    fn the_openai_field_names_map_onto_the_request() {
        let r: SpeakReq = serde_json::from_value(serde_json::json!({
            "model": "maya1",
            "input": "Hello!",
            "voice": "onyx",
            "instructions": "Very deep, very slow",
            "response_format": "wav"
        }))
        .unwrap();
        assert_eq!(r.text(), Some("Hello!"));
        // instructions is the description
        assert_eq!(r.description(), Some("Very deep, very slow"));
        assert_eq!(r.wire_format(), "wav");
    }

    #[test]
    fn unknown_formats_and_speed_are_refused_in_prepare() {
        let raw: serde_json::Value =
            serde_json::from_slice(config::APP_CONFIG_JSON).unwrap();
        let mut r = SpeakReq { input: Some("hi".into()), ..Default::default() };
        r.response_format = Some("mp3".into());
        let e = prepare(&raw, &r).err().unwrap();
        assert!(e.contains("[bad_format]"), "{e}");
        let r2 = SpeakReq { input: Some("hi".into()), speed: Some(1.5), ..Default::default() };
        let e = prepare(&raw, &r2).err().unwrap();
        assert!(e.contains("speed"), "{e}");
        // speed 1.0 is not a refusal (SDKs send the default explicitly)...
        let r3 = SpeakReq { input: Some("hi".into()), speed: Some(1.0), ..Default::default() };
        let e = prepare(&raw, &r3).err().unwrap();
        assert!(!e.contains("speed"), "{e}");
        // ...it fails later, on the missing model volume (no /models on a dev box)
        assert!(e.contains("volume") || e.contains("model"), "{e}");
    }

    #[test]
    fn empty_text_is_named_before_any_model_work() {
        let raw: serde_json::Value =
            serde_json::from_slice(config::APP_CONFIG_JSON).unwrap();
        let e = prepare(&raw, &SpeakReq::default()).err().unwrap();
        assert!(e.contains("[no_text]"), "{e}");
        let r = SpeakReq { input: Some("\u{7}\u{8}".into()), ..Default::default() };
        let e = prepare(&raw, &r).err().unwrap();
        assert!(e.contains("[no_text]"), "{e}");
    }

    #[test]
    fn error_codes_lift_cleanly() {
        assert_eq!(strip_code("[no_text] say something"), "say something");
        assert_eq!(err_status("[sessions_busy] busy"), 503);
        assert_eq!(err_status("[no_text] x"), 400);
        assert_eq!(err_status("tokenizer exploded"), 500);
    }
}
