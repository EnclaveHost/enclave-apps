//! speech-reader: speech to text on Enclave's wasi-nn GPU interface, as a
//! wasm component with a built-in web UI.
//!
//! MODEL: Granite Speech 4.1 2B (ibm-granite, Apache-2.0) - at 5.33% mean WER
//! the top open-weights model on the Open ASR leaderboard, and, decisively
//! for this fleet, llama.cpp-shaped: a conformer audio encoder + window
//! q-former projector (the volume's *mmproj*.gguf) feeding a dense granite
//! LM with the speech adapters already merged. Six languages (en fr de es pt
//! ja), transcription with punctuation, and speech translation to English.
//!
//! THE SPLIT: audio crosses to the host as FILE BYTES through the mtmd
//! "audio" verb - the exact mirror of image-reader's "image" verb, specced
//! for the platform in PLATFORM.md - and comes back as the POSITIONS it
//! consumed. The guest renders the model's own (bare) prompt frame around
//! it, decodes greedily, and streams the transcript. Decoding, resampling,
//! log-mels, the encoder and the projector all live host-side; this
//! component carries no DSP beyond reading WAV headers.
//!
//! LONG AUDIO: Granite Speech is trained at 4096 positions, so a long WAV is
//! cut at quiet points into ~4-minute episodes, each transcribed in a fresh
//! session; segments and their offsets are reported. Compressed inputs
//! (mp3/flac) pass whole - their duration is not knowable cheaply here - and
//! are admitted by byte count instead.
//!
//! Routes:
//!   GET  /            - the playground (record from the mic, or drop a file).
//!   GET  /ping        - liveness. Touches no wasi-nn, never authenticated.
//!   GET  /health      - volumes, budgets, node tuning; ?probe=1 opens a real
//!                       session and asks the HOST whether this node can hear.
//!   GET  /models      - the catalog.
//!   POST /transcribe  - the playground's SSE engine: raw audio body (or
//!                       multipart, or JSON {audio: base64}), {status}/{delta}
//!                       events while it works, {done, ...stats} at the end.
//!   GET  /v1/models   - OpenAI-shaped catalog.               (Bearer-gated)
//!   POST /v1/audio/transcriptions - OpenAI-compatible: multipart with `file`,
//!                       `response_format` json|text|verbose_json. `prompt`
//!                       REPLACES the task instruction (documented - on this
//!                       model the instruction IS the task).   (Bearer-gated)
//!   POST /v1/audio/translations   - same shape, translate-to-English task.
//!
//! PRIVACY: wasi:nn and the INCOMING http handler are the world's only
//! imports - no outbound socket exists, so a recording sent here cannot be
//! forwarded anywhere, is never written anywhere (the model volume is the
//! only mount, read-only), and exists in enclave RAM for the length of one
//! request. The attestation covers the component; verify, don't believe.

#[allow(warnings)]
mod bindings;

mod audio;
mod config;
mod multipart;
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
use nn::{now_ms, Clip, TranscribeParams};
use sampling::SampleParams;

static UI_HTML: &str = include_str!("ui.html");

/// Body ceiling: the audio byte cap plus multipart/base64 overhead.
const MAX_BODY_BYTES: usize = 48 * 1024 * 1024;

// -------------------------------------------------------------- wire shapes --

#[derive(Clone, Copy, PartialEq)]
enum Task {
    Transcribe,
    Translate,
}

/// One transcription request, whatever transport spelling it arrived in.
struct SpeechReq {
    audio: Vec<u8>,
    model: Option<String>,
    /// replaces the task instruction outright when present
    instruction: Option<String>,
    task: Task,
    temperature: Option<f32>,
    response_format: String,
}

#[derive(Deserialize)]
struct JsonReq {
    /// base64 (or data: URI) audio
    audio: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    instruction: Option<String>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    response_format: Option<String>,
}

/// Assemble a SpeechReq from whichever of the three transports arrived:
/// multipart/form-data (what SDKs send), JSON with base64, or the raw audio
/// bytes themselves (what curl and the playground send).
fn parse_request(
    req: &IncomingRequest,
    body: Vec<u8>,
    query: &str,
    task: Task,
) -> Result<SpeechReq, String> {
    let ctype = req
        .headers()
        .get(&"content-type".to_string())
        .first()
        .and_then(|v| String::from_utf8(v.clone()).ok())
        .unwrap_or_default();
    let mut r = SpeechReq {
        audio: Vec::new(),
        model: None,
        instruction: None,
        task,
        temperature: None,
        response_format: "json".into(),
    };
    // query params work on every transport (the playground uses them so its
    // body can stay pure audio)
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            let v = percent_decode(v);
            match k {
                "model" => r.model = Some(v),
                "instruction" | "prompt" => r.instruction = Some(v),
                "temperature" => r.temperature = v.parse().ok(),
                "response_format" | "format" => r.response_format = v,
                "task" if v == "translate" => r.task = Task::Translate,
                _ => {}
            }
        }
    }
    if let Some(b) = multipart::boundary(&ctype) {
        let parts = multipart::parse(&body, &b)?;
        r.audio = parts
            .iter()
            .find(|p| p.name == "file" || p.name == "audio")
            .map(|p| p.data.clone())
            .ok_or("multipart form has no `file` part")?;
        if let Some(m) = multipart::field(&parts, "model") {
            r.model = Some(m.to_string());
        }
        if let Some(p) = multipart::field(&parts, "prompt") {
            r.instruction = Some(p.to_string());
        }
        if let Some(t) = multipart::field(&parts, "temperature") {
            r.temperature = t.parse().ok();
        }
        if let Some(f) = multipart::field(&parts, "response_format") {
            r.response_format = f.to_string();
        }
    } else if ctype.starts_with("application/json") {
        let j: JsonReq =
            serde_json::from_slice(&body).map_err(|e| format!("bad JSON: {e}"))?;
        r.audio = b64_decode(j.audio.split(',').next_back().unwrap_or(""))?;
        r.model = j.model.or(r.model);
        r.instruction = j.instruction.or(j.prompt).or(r.instruction);
        r.temperature = j.temperature.or(r.temperature);
        if let Some(f) = j.response_format {
            r.response_format = f;
        }
    } else {
        r.audio = body;
    }
    if r.audio.is_empty() {
        return Err("[no_audio] the request carries no audio - send the file bytes as the \
                    body, a multipart `file` part, or JSON {\"audio\": \"<base64>\"}"
            .into());
    }
    Ok(r)
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

/// Standard base64, tolerant of whitespace and missing padding.
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
            _ => return Err("audio payload is not valid base64".into()),
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    if out.is_empty() {
        return Err("audio payload is empty".into());
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
                        "request body exceeds {} MB - long recordings compress well (flac), \
                         and anything over the cap wants to be several requests",
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

/// Machine-readable conditions tagged "[code] " inside error messages, lifted
/// into `error.code` so a caller can branch without parsing prose.
const ERR_CODES: &[&str] = &[
    "no_audio", "audio_undecodable", "audio_too_long", "audio_unavailable",
    "audio_unsupported", "bad_format", "sessions_busy", "model_not_loaded",
    "host_load_failed", "volume_not_attached", "kv_pool_full",
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
        || e.contains("[kv_pool_full]")
    {
        503
    } else if e.contains("[audio_unsupported]") {
        501
    } else if e.contains('[') {
        400
    } else {
        500
    }
}

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

// ------------------------------------------------------------- transcription --

struct Prepared {
    cfg: AppConfig,
    tok: Tokenizer,
    params: TranscribeParams,
    /// total duration when knowable (WAV) - what verbose_json reports
    duration: Option<f32>,
}

/// Admission + chunking + prompt assembly, BEFORE any response stream opens.
fn prepare(raw: &serde_json::Value, r: &SpeechReq) -> Result<Prepared, String> {
    let cfg = nn::resolve_model(raw, r.model.as_deref())?;
    if r.audio.len() > cfg.max_audio_bytes {
        return Err(format!(
            "[audio_too_long] the file is {} MB; this deployment takes {} MB - flac halves \
             wav, and anything longer wants to be several requests",
            r.audio.len() / (1024 * 1024),
            cfg.max_audio_bytes / (1024 * 1024)
        ));
    }
    let kind = audio::sniff(&r.audio)?;
    let (clips, duration) = match kind {
        audio::Kind::Wav => {
            let w = audio::parse_wav(&r.audio)?;
            if w.seconds as usize > cfg.max_audio_seconds {
                return Err(format!(
                    "[audio_too_long] {:.0} minutes of audio; this deployment takes {} \
                     minutes per request",
                    w.seconds / 60.0,
                    cfg.max_audio_seconds / 60
                ));
            }
            match w.s16 {
                Some(s16) => {
                    let mono = audio::downmix(&s16, w.channels);
                    let clips: Vec<Clip> = audio::chunk_pcm(&mono, w.sample_rate, cfg.chunk_seconds)
                        .into_iter()
                        .map(|(pcm, secs)| Clip {
                            bytes: audio::wav_bytes(&pcm, w.sample_rate),
                            seconds: Some(secs),
                        })
                        .collect();
                    (clips, Some(w.seconds))
                }
                // an exotic wav encoding (float, adpcm): it cannot be cut
                // here, so it must fit ONE episode - the same positions
                // arithmetic the chunker is sized by
                None => {
                    let est = w.seconds as usize * cfg.audio_tokens_per_second;
                    if est + cfg.default_max_new / 2 > cfg.max_positions {
                        return Err(format!(
                            "[audio_too_long] {:.0}s of non-PCM wav cannot be chunked here \
                             (~{est} of the {} positions one episode holds) - re-encode as \
                             16-bit PCM (ffmpeg -i in -c:a pcm_s16le out.wav), which chunks",
                            w.seconds, cfg.max_positions
                        ));
                    }
                    (vec![Clip { bytes: r.audio.clone(), seconds: Some(w.seconds) }], Some(w.seconds))
                }
            }
        }
        // compressed: duration unknowable cheaply; the byte cap admitted it,
        // the model's position budget is the backstop
        audio::Kind::Mp3 | audio::Kind::Flac => {
            (vec![Clip { bytes: r.audio.clone(), seconds: None }], None)
        }
    };
    let tok_bytes = nn::read_tokenizer(&cfg)?;
    let tok = Tokenizer::from_bytes(&tok_bytes).map_err(|e| format!("tokenizer: {e}"))?;
    let instruction = r
        .instruction
        .clone()
        .unwrap_or_else(|| match r.task {
            Task::Transcribe => cfg.instruction.clone(),
            Task::Translate => cfg.translate_instruction.clone(),
        });
    let params = TranscribeParams {
        instruction,
        clips,
        sample: SampleParams {
            temperature: r.temperature.unwrap_or(cfg.temperature).clamp(0.0, 1.0),
            top_p: cfg.top_p.clamp(0.05, 1.0),
            top_k: cfg.top_k,
            rep_penalty: cfg.rep_penalty,
            rep_window: cfg.rep_window,
        },
        max_new: cfg.default_max_new.min(cfg.max_new_cap).max(16),
        loop_reps: cfg.repeat_guard,
    };
    Ok(Prepared { cfg, tok, params, duration })
}

fn stats_json(cfg: &AppConfig, s: &nn::TranscribeStats, duration: Option<f32>) -> serde_json::Value {
    let gen_s = (s.decode_ms as f64) / 1000.0;
    serde_json::json!({
        "model": cfg.name,
        "audio_positions": s.audio_pos,
        "segments": s.segments.len(),
        "duration_seconds": duration,
        "prompt_tokens": s.prompt_tokens,
        "tokens": s.tokens,
        "finish_reason": s.finish_reason,
        "load_ms": s.load_ms as u64,
        "prefill_ms": s.prefill_ms as u64,
        "decode_ms": s.decode_ms as u64,
        "ms": (s.load_ms + s.prefill_ms + s.decode_ms) as u64,
        "tok_per_s": if gen_s > 0.0 { ((s.tokens as f64 / gen_s) * 10.0).round() / 10.0 } else { 0.0 },
    })
}

// --------------------------------------------------- POST /transcribe (SSE) --

fn handle_transcribe_sse(raw: &serde_json::Value, r: SpeechReq, out: ResponseOutparam) {
    let prep = match prepare(raw, &r) {
        Ok(p) => p,
        Err(e) => return json_err(out, err_status(&e), &e),
    };
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
    match nn::transcribe(&prep.cfg, &prep.tok, &prep.params, &emit, &status) {
        Ok(s) => {
            let mut done = stats_json(&prep.cfg, &s, prep.duration);
            done["done"] = serde_json::json!(true);
            done["text"] = serde_json::json!(s.text);
            send(done);
        }
        Err(e) => {
            send(serde_json::json!({ "error": strip_code(&e) }));
        }
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// ------------------------------------- POST /v1/audio/{transcriptions,translations} --

/// OpenAI-shaped, non-SSE. The subtlety is the fleet's gateway, which cuts a
/// response ~180 s after its last byte: a long recording is minutes of work,
/// so the response opens IMMEDIATELY and newline keepalives trickle ahead of
/// the JSON - leading whitespace is valid JSON, and every SDK's .json() eats
/// it without noticing. (For response_format=text the newlines lead the text;
/// a transcript consumer that cannot tolerate a leading blank line trims it.)
fn handle_openai(raw: &serde_json::Value, r: SpeechReq, out: ResponseOutparam) {
    let format = r.response_format.clone();
    if !matches!(format.as_str(), "json" | "text" | "verbose_json") {
        return json_err(
            out,
            400,
            &format!(
                "[bad_format] response_format '{format}' is not available here: json, text \
                 and verbose_json are. (srt/vtt need word timestamps, which this model \
                 variant does not emit.)"
            ),
        );
    }
    let prep = match prepare(raw, &r) {
        Ok(p) => p,
        Err(e) => return json_err(out, err_status(&e), &e),
    };
    let headers = Fields::new();
    let ctype: &[u8] =
        if format == "text" { b"text/plain; charset=utf-8" } else { b"application/json" };
    let _ = headers.set(&"content-type".to_string(), &[ctype.to_vec()]);
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
    let quiet_emit = |_: &str| true;
    let keepalive = |_: &str| write_all(b"\n");
    let result = nn::transcribe(&prep.cfg, &prep.tok, &prep.params, &quiet_emit, &keepalive);
    match result {
        Ok(s) => match format.as_str() {
            "text" => {
                let _ = write_all(s.text.as_bytes());
                let _ = write_all(b"\n");
            }
            "verbose_json" => {
                let segments: Vec<serde_json::Value> = s
                    .segments
                    .iter()
                    .enumerate()
                    .map(|(i, seg)| {
                        serde_json::json!({
                            "id": i,
                            "start": seg.start,
                            "end": seg.start.zip(seg.seconds).map(|(a, b)| a + b),
                            "text": seg.text,
                        })
                    })
                    .collect();
                let body = serde_json::json!({
                    "task": if r.task == Task::Translate { "translate" } else { "transcribe" },
                    "duration": prep.duration,
                    "text": s.text,
                    "segments": segments,
                    "enclave": stats_json(&prep.cfg, &s, prep.duration),
                });
                let _ = write_all(body.to_string().as_bytes());
            }
            _ => {
                let body = serde_json::json!({
                    "text": s.text,
                    "enclave": stats_json(&prep.cfg, &s, prep.duration),
                });
                let _ = write_all(body.to_string().as_bytes());
            }
        },
        Err(e) => {
            // the stream is already 200; the error still arrives in the shape
            // the SDK parses, with the code lifted out
            let code = ERR_CODES.iter().copied().find(|c| e.contains(&format!("[{c}] ")));
            let mut err = serde_json::json!({ "message": strip_code(&e), "type": "server_error" });
            if let Some(c) = code {
                err["code"] = serde_json::json!(c);
            }
            let _ = write_all(serde_json::json!({ "error": err }).to_string().as_bytes());
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
                "hears": e.mmproj.is_some(),
                "languages": ["en", "fr", "de", "es", "pt", "ja"],
                "chunk_seconds": e.cfg.chunk_seconds,
                "max_audio_seconds": e.cfg.max_audio_seconds,
                "default": Some(&e.cfg.name) == default.as_ref(),
                "fits": !unfit.contains_key(&e.volume),
            });
            if let Some(why) = unfit.get(&e.volume) {
                row["why"] = serde_json::json!(why);
            }
            if e.mmproj.is_none() {
                row["why_deaf"] = serde_json::json!(
                    "this volume carries no *mmproj*.gguf, so it holds a language model and \
                     nothing to hear with - rewrap the volume with its audio encoder included"
                );
            }
            row
        })
        .collect();
    (rows, default)
}

fn handle_models(raw: &serde_json::Value, out: ResponseOutparam) {
    let (rows, _) = model_rows(raw);
    let base = raw.get("name").and_then(|n| n.as_str()).unwrap_or("speech-reader");
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
    respond_bytes(
        out,
        200,
        "application/json",
        serde_json::json!({ "object": "list", "data": data }).to_string().as_bytes(),
    );
}

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
        "app": "speech-reader",
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
                            "ok": c.audio == Some(true),
                            "model": cfg.name,
                            "host_audio": c.audio,
                            "note": match c.audio {
                                Some(true) => "the host reports audio support for this \
                                               volume: recordings will transcribe",
                                Some(false) => "the host reports NO audio support: either \
                                                the volume carries no audio *mmproj*.gguf, \
                                                or this node's shim predates the audio verb \
                                                (PLATFORM.md is the spec)",
                                None => "this node's host answers the caps verb but is too \
                                         old to have an audio slot - its shim predates the \
                                         audio verb (PLATFORM.md is the spec)",
                            }
                        }),
                        Err(e) => serde_json::json!({
                            "ok": false, "model": cfg.name,
                            "note": format!(
                                "this node's host does not answer the capability verb at \
                                 all; it long predates audio support ({})",
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
            (Method::Post, "/transcribe") => {
                let body = match read_body(&req) {
                    Ok(b) => b,
                    Err(e) => return json_err(out, 400, &e),
                };
                return match parse_request(&req, body, query, Task::Transcribe) {
                    Ok(r) => handle_transcribe_sse(&raw, r, out),
                    Err(e) => json_err(out, err_status(&e), &e),
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
        let task = match path {
            "/v1/audio/translations" | "/audio/translations" => Task::Translate,
            _ => Task::Transcribe,
        };
        match (method, path) {
            (Method::Get, "/v1/models") => handle_models(&raw, out),
            (Method::Post, "/v1/audio/transcriptions")
            | (Method::Post, "/audio/transcriptions")
            | (Method::Post, "/v1/audio/translations")
            | (Method::Post, "/audio/translations") => {
                let body = match read_body(&req) {
                    Ok(b) => b,
                    Err(e) => return json_err(out, 400, &e),
                };
                match parse_request(&req, body, query, task) {
                    Ok(r) => handle_openai(&raw, r, out),
                    Err(e) => json_err(out, err_status(&e), &e),
                }
            }
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /ping, GET /health, GET /models, \
                 POST /transcribe, GET /v1/models, POST /v1/audio/transcriptions, \
                 POST /v1/audio/translations",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_data_uris_decode() {
        assert_eq!(b64_decode("UklGRg==").unwrap(), b"RIFF");
        assert_eq!(b64_decode("UklG Rg\n==").unwrap(), b"RIFF");
        assert!(b64_decode("!!!").is_err());
    }

    #[test]
    fn error_codes_lift_and_map_to_statuses() {
        assert_eq!(strip_code("[no_audio] send audio"), "send audio");
        assert_eq!(err_status("[sessions_busy] x"), 503);
        assert_eq!(err_status("[audio_unsupported] x"), 501);
        assert_eq!(err_status("[audio_too_long] x"), 400);
        assert_eq!(err_status("boom"), 500);
    }

    #[test]
    fn the_query_string_reaches_every_transport() {
        // task=translate and instruction override ride the query even when
        // the body is raw audio
        let q = "task=translate&instruction=translate+the+speech+to+English.&temperature=0.2";
        let mut r = SpeechReq {
            audio: vec![1],
            model: None,
            instruction: None,
            task: Task::Transcribe,
            temperature: None,
            response_format: "json".into(),
        };
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                let v = percent_decode(v);
                match k {
                    "instruction" | "prompt" => r.instruction = Some(v),
                    "temperature" => r.temperature = v.parse().ok(),
                    "task" if v == "translate" => r.task = Task::Translate,
                    _ => {}
                }
            }
        }
        assert!(r.task == Task::Translate);
        assert_eq!(r.instruction.as_deref(), Some("translate the speech to English."));
        assert_eq!(r.temperature, Some(0.2));
    }
}
