//! llm-chat: a general-purpose LLM service compiled into a wasm component,
//! running on Enclave's wasi-nn GPU interface. Ships NO weights - models
//! arrive as attached Modelwrap volumes, and every attached volume the
//! config's `models` catalog describes is servable, largest one the default
//! (see config.rs). Geometry, chat template, sampling defaults and the API
//! key are configuration, not code - a deployment can override any of it via
//! ENCLAVE_CONFIG (the platform passes the deployment's CID-verified
//! configCid JSON through the tenant environment).
//!
//! Routes:
//!   GET  /                    - chat playground (self-contained HTML).
//!   GET  /emoji.woff2         - color-emoji fallback font (Noto COLRv1): the
//!                               playground declares it with local() sources
//!                               first + unicode-range, so a browser only
//!                               downloads it when the system has no emoji
//!                               font AND a reply actually contains emoji.
//!   GET  /ping                - liveness, touches no wasi-nn.
//!   GET  /models              - the servable models (attached volumes the
//!                               config describes), largest first; the
//!                               largest is the default. Open, unlike
//!                               /v1/models - the playground dropdown reads it.
//!   GET  /warmup              - warm models before the first prompt. With
//!                               ?model=<name|volume>: that one (load + one
//!                               forward pass). BARE - the manager's boot
//!                               warmup and the playground's page load - it
//!                               is a LADDER: every servable model tried
//!                               SMALLEST-FIRST, one at a time; a model that
//!                               does not fit the share is reported unfit
//!                               and skipped, not fatal, so one published
//!                               app serves whatever the deployment can hold
//!                               (the playground disables the rest in its
//!                               menu). GPU-only unless ?target= says
//!                               otherwise.
//!   GET  /v1/models           - OpenAI-compatible model list.
//!   POST /v1/chat/completions - OpenAI-compatible completions, stream and
//!                               non-stream. Point any OpenAI SDK at the
//!                               deployment URL. If the config sets api_key,
//!                               requires `Authorization: Bearer <key>`.
//!   POST /chat                - legacy SSE endpoint used by the playground.
//!
//! Generation: autoregressive decode with the model's KV cache. The trick
//! that makes this cheap through wasi-nn: `compute()` returns OWNED tensor
//! resources for the `present.*` KV tensors, and we hand those handles
//! straight back as the next step's `past_key_values.*` inputs - the cache
//! bytes never cross into guest memory. Only the logits are read out
//! (one vocab row per decode step).
#[allow(warnings)]
mod bindings;

mod config;
mod sampling;

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokenizers::Tokenizer;

use bindings::exports::wasi::http::incoming_handler::Guest;
use bindings::wasi::http::types::{
    Fields, IncomingRequest, Method, OutgoingBody, OutgoingResponse, ResponseOutparam,
};
use bindings::wasi::io::streams::StreamError;
use bindings::wasi::nn::graph::{load, load_by_name, ExecutionTarget, GraphEncoding};
use bindings::wasi::nn::inference::GraphExecutionContext;
use bindings::wasi::nn::tensor::{Tensor, TensorType};

use config::AppConfig;
use sampling::{pick_token, Rng, SampleParams};

static CHAT_HTML: &str = include_str!("chat.html");
static EMOJI_WOFF2: &[u8] = include_bytes!("../assets/emoji.woff2");

// ------------------------------------------------------------ model volumes --
// Weights + tokenizer arrive as ATTACHED MODEL VOLUMES (Tinfoil Modelwrap):
// the platform preopens each attached volume read-only at /models/<name> and
// lists the names in ENCLAVE_MODELS. The app embeds NO weights - the config's
// `models` catalog describes the volumes it can serve (see config.rs and
// available_models below), so ONE published wasm serves whatever the
// deployment mounts. The host caches the ORT session by graph bytes and
// preloads GGUF graphs at boot, so re-reading per request only pays real
// cost on the first load after a node boot.
const MODELS_ROOT: &str = "/models";

fn attached_volumes() -> Vec<String> {
    std::env::var("ENCLAVE_MODELS")
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Resolve a config path against the model volume: absolute paths are used
/// verbatim (cross-VOLUME reads - e.g. a tokenizer living in a sibling
/// volume when the weights repo carries none), everything else is
/// volume-relative.
fn volume_path(root: &PathBuf, rel: &str) -> PathBuf {
    if rel.starts_with('/') { PathBuf::from(rel) } else { root.join(rel) }
}

/// Read one file out of the configured model volume. The `explicit` config
/// path is tried first, then the conventional candidate names - a fallback,
/// not a replacement, so one catalog entry can serve both a volume that
/// carries the file and one that needs the explicit (possibly cross-volume)
/// location.
fn volume_file(
    cfg: &AppConfig,
    explicit: &Option<String>,
    candidates: &[&str],
    what: &str,
) -> Result<Vec<u8>, String> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    if !root.is_dir() {
        let have = attached_volumes();
        return Err(format!(
            "model volume '{}' is not attached at {MODELS_ROOT}/{} (attached: {}) - \
             deploy with {{\"volumes\":[\"{}\"]}} in the config, or tick it in the console's volume picker",
            cfg.model_volume,
            cfg.model_volume,
            if have.is_empty() { "none".to_string() } else { have.join(", ") },
            cfg.model_volume,
        ));
    }
    let mut rels: Vec<String> = Vec::new();
    rels.extend(explicit.iter().cloned());
    rels.extend(candidates.iter().map(|s| s.to_string()));
    for rel in &rels {
        let p = volume_path(&root, rel);
        if p.is_file() {
            return std::fs::read(&p).map_err(|e| format!("reading {}: {e}", p.display()));
        }
    }
    Err(format!(
        "no {what} in volume '{}' (tried: {})",
        cfg.model_volume,
        rels.join(", ")
    ))
}

const ONNX_CANDIDATES: &[&str] =
    &["onnx/model_q4.onnx", "model_q4.onnx", "onnx/model.onnx", "model.onnx"];

fn read_model(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    volume_file(cfg, &cfg.model_file, ONNX_CANDIDATES, "ONNX model")
}

fn read_tokenizer(cfg: &AppConfig) -> Result<Vec<u8>, String> {
    volume_file(cfg, &cfg.tokenizer_file, &["tokenizer.json"], "tokenizer.json")
}

/// The split-GGUF family covering `path` (llama.cpp's
/// "<prefix>-NNNNN-of-MMMMM.gguf" convention, forced on >50GB models by HF's
/// per-file cap), every part present - or None if `path` isn't a split part
/// or a sibling is missing. The host loads part 00001 and derives the sibling
/// paths from its name; the model's true size is the sum of its parts.
fn split_family(path: &Path) -> Option<Vec<PathBuf>> {
    let name = path.file_name()?.to_str()?;
    let stem = name.strip_suffix(".gguf")?;
    let (rest, count) = stem.rsplit_once("-of-")?;
    let (prefix, no) = rest.rsplit_once('-')?;
    if no.len() != 5 || count.len() != 5 || no.parse::<u32>().is_err() {
        return None;
    }
    let n = count.parse::<u32>().ok().filter(|n| *n >= 1)?;
    let dir = path.parent()?;
    let parts: Vec<PathBuf> = (1..=n)
        .map(|i| dir.join(format!("{prefix}-{i:05}-of-{count}.gguf")))
        .collect();
    parts.iter().all(|p| p.is_file()).then_some(parts)
}

/// Locate `cfg`'s weights file WITHOUT reading it: powers the availability
/// listing and the size ranking that picks the default model. Mirrors the
/// lookup the backends do for real. For ggml, `model_file` names the gguf in
/// a multi-quant volume (keep it in step with the host's MODEL_VOLUMES pick -
/// the host decides what actually preloads); otherwise the host's
/// model.gguf / single-*.gguf / one-split-family contract applies. A split
/// model reports the SUM of its parts - part 00001 alone is a header-sized
/// sliver, and this ranking picks the default model.
fn weights_size(cfg: &AppConfig) -> Option<u64> {
    let root = PathBuf::from(MODELS_ROOT).join(&cfg.model_volume);
    if cfg.backend == "ggml" {
        let path = if let Some(f) = &cfg.model_file {
            volume_path(&root, f)
        } else {
            let preferred = root.join("model.gguf");
            if preferred.is_file() {
                preferred
            } else {
                let mut ggufs: Vec<PathBuf> = std::fs::read_dir(&root)
                    .ok()?
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().map(|x| x == "gguf").unwrap_or(false) && p.is_file())
                    .collect();
                match ggufs.len() {
                    0 => return None,
                    1 => ggufs.pop()?,
                    n => {
                        // one complete split family covering every gguf, else ambiguous
                        let first = ggufs
                            .iter()
                            .find(|p| {
                                split_family(p).is_some_and(|fam| fam.len() == n)
                            })?
                            .clone();
                        first
                    }
                }
            }
        };
        let parts = split_family(&path);
        let files = parts.as_deref().unwrap_or(std::slice::from_ref(&path));
        let mut total = 0u64;
        for f in files {
            total += std::fs::metadata(f).ok()?.len();
        }
        return Some(total);
    }
    let mut rels: Vec<String> = Vec::new();
    rels.extend(cfg.model_file.iter().cloned());
    rels.extend(ONNX_CANDIDATES.iter().map(|s| s.to_string()));
    rels.iter()
        .map(|rel| volume_path(&root, rel))
        .find(|p| p.is_file())
        .and_then(|p| std::fs::metadata(&p).ok().map(|m| m.len()))
}

// ------------------------------------------------------------ model choice --

/// One servable model: an attached volume the config knows how to describe.
struct ModelEntry {
    volume: String,
    bytes: u64,
    cfg: AppConfig,
}

/// The servable models: every attached volume with a `models` catalog entry
/// (or equal to the top-level model_volume, which the top-level config
/// describes by itself). Sorted by weights size, LARGEST FIRST - index 0 is
/// the default, so a deployment that attaches several models serves the
/// biggest unless a request names another. Volumes that are attached but
/// undescribed (unknown geometry/template) or missing their weights file are
/// skipped - they cannot be served, only misserved.
fn available_models(raw: &serde_json::Value) -> Vec<ModelEntry> {
    let top_volume = raw.get("model_volume").and_then(|v| v.as_str()).unwrap_or("");
    let catalog = raw.get("models").and_then(|m| m.as_object());
    let mut out = Vec::new();
    for vol in attached_volumes() {
        let entry = match catalog.and_then(|m| m.get(&vol)) {
            Some(e) => e.clone(),
            None if vol == top_volume => serde_json::json!({}),
            None => continue,
        };
        let Ok(cfg) = config::resolve_entry(raw, &vol, entry) else { continue };
        let Some(bytes) = weights_size(&cfg) else { continue };
        out.push(ModelEntry { volume: vol, bytes, cfg });
    }
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.volume.cmp(&b.volume)));
    out
}

/// The deployment's VRAM budget in bytes: ENCLAVE_VRAM_BYTES, set by the
/// platform from gpuShare x card VRAM - the same number the MPS cap
/// enforces on this process. None on CPU deployments and older managers.
fn vram_budget() -> Option<u64> {
    std::env::var("ENCLAVE_VRAM_BYTES")
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|b| *b > 0)
}

/// Which servable models CANNOT fit the VRAM budget - the manager's preload
/// rule, mirrored: models claim the budget smallest-first (the preload /
/// warmup order), so a model is certainly-unfit once the smaller models'
/// weights plus its own exceed the budget. Weights-only on purpose:
/// contexts and compute buffers come and go per request, so this gate only
/// refuses CERTAIN failures - borderline models still get the honest
/// warmup probe. Returns volume -> reason, for the unfit models only.
fn over_budget(entries: &[ModelEntry]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(budget) = vram_budget() else { return out };
    let mut asc: Vec<&ModelEntry> = entries.iter().collect();
    // same order (and tie-break) as the manager's preload emission, so both
    // sides always agree on which model crosses the budget line
    asc.sort_by(|a, b| a.bytes.cmp(&b.bytes).then(a.volume.cmp(&b.volume)));
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut claimed = 0u64;
    for e in asc {
        if claimed + e.bytes > budget {
            out.insert(
                e.volume.clone(),
                format!(
                    "{:.1} GB of weights cannot fit the deployment's {:.1} GB VRAM budget\
                     {} - redeploy with a larger GPU share to unlock this model",
                    gb(e.bytes),
                    gb(budget),
                    if claimed > 0 {
                        format!(" ({:.1} GB already claimed by smaller models)", gb(claimed))
                    } else {
                        String::new()
                    }
                ),
            );
        } else {
            claimed += e.bytes;
        }
    }
    out
}

/// The AppConfig serving one request. `requested` (the OpenAI `model` field,
/// or ?model= on /warmup) matches a model name or volume name; an UNKNOWN
/// name falls back to the default model instead of erroring - OpenAI SDKs
/// require a model string and clients routinely send one this deployment
/// never heard of. No servable models at all falls back to the top-level
/// config, whose volume-not-attached error path tells the operator what to
/// attach.
fn resolve_model(raw: &serde_json::Value, requested: Option<&str>) -> Result<AppConfig, String> {
    let entries = available_models(raw);
    if let Some(want) = requested {
        if let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) {
            return Ok(e.cfg.clone());
        }
    }
    match entries.into_iter().next() {
        Some(e) => Ok(e.cfg),
        None => config::from_value(raw.clone()),
    }
}

const PREFILL_CHUNK: usize = 128;
const MAX_BODY_BYTES: usize = 256 * 1024;

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------- tensors --

fn i64_tensor(dims: &[u32], vals: &[i64]) -> Tensor {
    let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_le_bytes()).collect();
    Tensor::new(dims, TensorType::I64, &bytes)
}

fn empty_past(cfg: &AppConfig) -> Vec<(String, Tensor)> {
    let mut past = Vec::with_capacity((cfg.n_layers * 2) as usize);
    for l in 0..cfg.n_layers {
        for kind in ["key", "value"] {
            past.push((
                format!("past_key_values.{l}.{kind}"),
                Tensor::new(&[1, cfg.n_kv_heads, 0, cfg.head_dim], TensorType::Fp32, &[]),
            ));
        }
    }
    past
}

fn nn_err(stage: &str, e: bindings::wasi::nn::errors::Error) -> String {
    format!("{stage}: {:?}: {}", e.code(), e.data())
}

// ------------------------------------------------------------- generation --

struct StepResult {
    logits: Vec<f32>,
    past: Vec<(String, Tensor)>,
}

/// One forward pass. `past` is consumed (the host drops the old cache).
fn step(
    cfg: &AppConfig,
    ctx: &GraphExecutionContext,
    ids: &[i64],
    past: Vec<(String, Tensor)>,
    past_len: usize,
    read_logits: bool,
) -> Result<StepResult, String> {
    let new_len = ids.len();
    let total = past_len + new_len;
    let mut inputs: Vec<(String, Tensor)> = Vec::with_capacity(3 + past.len());
    inputs.push(("input_ids".into(), i64_tensor(&[1, new_len as u32], ids)));
    inputs.push((
        "attention_mask".into(),
        i64_tensor(&[1, total as u32], &vec![1i64; total]),
    ));
    let positions: Vec<i64> = (past_len as i64..total as i64).collect();
    inputs.push((
        "position_ids".into(),
        i64_tensor(&[1, new_len as u32], &positions),
    ));
    inputs.extend(past);

    let outputs = ctx.compute(inputs).map_err(|e| nn_err("compute", e))?;

    let mut logits = Vec::new();
    let mut next_past = Vec::with_capacity((cfg.n_layers * 2) as usize);
    for (name, tensor) in outputs {
        if name == "logits" {
            if read_logits {
                let data = tensor.data();
                let row = cfg.vocab * 4;
                if data.len() < row {
                    return Err(format!("logits too short: {} bytes", data.len()));
                }
                let tail = &data[data.len() - row..];
                logits = tail
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect();
            }
        } else if let Some(rest) = name.strip_prefix("present.") {
            next_past.push((format!("past_key_values.{rest}"), tensor));
        }
    }
    if next_past.len() != (cfg.n_layers * 2) as usize {
        return Err(format!(
            "expected {} KV outputs, got {} - do the config's n_layers/n_kv_heads match the model?",
            cfg.n_layers * 2,
            next_past.len()
        ));
    }
    if read_logits && logits.len() != cfg.vocab {
        return Err("model returned no logits (config vocab mismatch?)".into());
    }
    Ok(StepResult { logits, past: next_past })
}

/// A live inference session. The two backends differ in WHERE the KV cache
/// lives: ONNX shuttles past_key_values tensors through every call (the cache
/// crosses the guest boundary), while ggml keeps it host-side inside the
/// execution context - the guest feeds token ids and reads one logits row.
/// ggml models are HOST-PRELOADED (-S nn-graph=ggml::<volume dir>), so
/// load_by_name(model_volume) never pulls weights into guest memory and the
/// model size is bounded by the deployment's share, not wasm32.
enum Session {
    Onnx { ctx: GraphExecutionContext, past: Vec<(String, Tensor)>, total: usize },
    Ggml { ctx: GraphExecutionContext },
}

impl Session {
    fn open(cfg: &AppConfig, target: ExecutionTarget) -> Result<Session, String> {
        match cfg.backend.as_str() {
            "ggml" => {
                let graph = load_by_name(&cfg.model_volume).map_err(|e| {
                    format!(
                        "{} (is the \"{}\" volume attached, and does it carry a GGUF? \
                         ggml needs a GPU-share deployment - the host preloads the model)",
                        nn_err("load_by_name", e),
                        cfg.model_volume
                    )
                })?;
                let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
                Ok(Session::Ggml { ctx })
            }
            "onnx" => {
                let model = read_model(cfg)?;
                let graph =
                    load(&[model], GraphEncoding::Onnx, target).map_err(|e| nn_err("load", e))?;
                let ctx = graph.init_execution_context().map_err(|e| nn_err("init", e))?;
                Ok(Session::Onnx { ctx, past: empty_past(cfg), total: 0 })
            }
            other => Err(format!("unknown backend \"{other}\" (expected \"onnx\" or \"ggml\")")),
        }
    }

    /// Feed `ids`; with `want_logits`, return the LAST token's logits row.
    fn feed(&mut self, cfg: &AppConfig, ids: &[u32], want_logits: bool) -> Result<Vec<f32>, String> {
        match self {
            Session::Onnx { ctx, past, total } => {
                let ids64: Vec<i64> = ids.iter().map(|&t| t as i64).collect();
                let r = step(cfg, ctx, &ids64, std::mem::take(past), *total, want_logits)?;
                *past = r.past;
                *total += ids.len();
                Ok(r.logits)
            }
            Session::Ggml { ctx } => {
                let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
                let outs = ctx
                    .compute(vec![(
                        "tokens".to_string(),
                        Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes),
                    )])
                    .map_err(|e| nn_err("compute", e))?;
                if !want_logits {
                    return Ok(Vec::new());
                }
                let logits = outs
                    .iter()
                    .find(|(n, _)| n == "logits")
                    .ok_or("ggml backend returned no \"logits\" output")?;
                let data = logits.1.data();
                if data.len() != cfg.vocab * 4 {
                    return Err(format!(
                        "ggml logits are {} bytes, config vocab says {} - wrong model_volume for this config?",
                        data.len(),
                        cfg.vocab * 4
                    ));
                }
                Ok(data
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect())
            }
        }
    }
}

struct GenParams {
    max_new: usize,
    sample: SampleParams,
    stop_strings: Vec<String>,
}

struct GenStats {
    target: String,
    prompt_tokens: usize,
    tokens: usize,
    load_ms: u128,
    prefill_ms: u128,
    decode_ms: u128,
    finish_reason: &'static str,
    text: String,
}

/// Run the full completion; `emit` receives text deltas as they stabilize
/// (with a holdback of the longest stop string so a stop sequence is never
/// partially emitted), `status` receives progress lines. Both return false
/// when the client is gone. Status events double as keepalive bytes during
/// the one long silence (cold session init; the host caches sessions).
fn generate(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    target: ExecutionTarget,
    tname: &str,
    p: &GenParams,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    if !status(&format!(
        "loading the model on {tname} - the first request after a node boot initializes the session and can take a while"
    )) {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    let mut sess = Session::open(cfg, target)?;
    let load_ms = now_ms() - t0;
    if !status(&format!(
        "session ready ({load_ms} ms); prefilling {} prompt tokens",
        prompt_ids.len()
    )) {
        return Err("client disconnected".into());
    }

    // -- prefill, in chunks so no single logits tensor gets huge
    let t1 = now_ms();
    let mut done = 0usize;
    let mut logits = Vec::new();
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let last = end == prompt_ids.len();
        let l = sess.feed(cfg, &prompt_ids[done..end], last)?;
        if last {
            logits = l;
        }
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    // -- decode
    let t2 = now_ms();
    let holdback = p.stop_strings.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut generated: Vec<u32> = Vec::new();
    let mut emitted = 0usize; // chars of decoded text already sent
    let mut finish: &'static str = "stop";
    let mut final_text = String::new();
    loop {
        let recent: Vec<u32> = if generated.is_empty() {
            prompt_ids[prompt_ids.len().saturating_sub(p.sample.rep_window)..].to_vec()
        } else {
            generated[generated.len().saturating_sub(p.sample.rep_window)..].to_vec()
        };
        let next = pick_token(&mut logits, &recent, &p.sample, &mut rng);
        if cfg.eos.contains(&next) {
            break;
        }
        if generated.len() >= p.max_new {
            finish = "length";
            break;
        }
        generated.push(next);

        // incremental detokenization: decode everything, emit the stable
        // suffix minus the stop-string holdback; hold while the tail is an
        // incomplete UTF-8 sequence
        if let Ok(text) = tok.decode(&generated, true) {
            // stop-string scan on the full decoded text
            if let Some(pos) = p
                .stop_strings
                .iter()
                .filter_map(|s| text.find(s.as_str()))
                .min()
            {
                final_text = text[..pos].to_string();
                if pos > emitted {
                    if !emit(&text[emitted..pos]) {
                        return Err("client disconnected".into());
                    }
                }
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(),
                    prompt_tokens: prompt_ids.len(),
                    tokens: generated.len(),
                    load_ms,
                    prefill_ms,
                    decode_ms,
                    finish_reason: "stop",
                    text: final_text,
                });
            }
            let visible = text.len().saturating_sub(holdback);
            if !text.ends_with('\u{FFFD}') && visible > emitted {
                if let Some(delta) = text.get(emitted..visible) {
                    if !emit(delta) {
                        break; // client disconnected
                    }
                    emitted = visible;
                }
            }
            final_text = text;
        }

        logits = sess.feed(cfg, &[next], true)?;
    }
    // flush whatever the holdback was withholding
    if final_text.len() > emitted {
        if let Some(delta) = final_text.get(emitted..) {
            let _ = emit(delta);
        }
    }
    let decode_ms = now_ms() - t2;

    Ok(GenStats {
        target: tname.to_string(),
        prompt_tokens: prompt_ids.len(),
        tokens: generated.len(),
        load_ms,
        prefill_ms,
        decode_ms,
        finish_reason: finish,
        text: final_text,
    })
}

// -------------------------------------------------------------------- http --

#[derive(Deserialize)]
struct ChatMsg {
    role: String,
    content: String,
}

/// Request shape shared by /chat (legacy) and /v1/chat/completions (OpenAI).
/// OpenAI fields we don't implement are accepted and ignored.
#[derive(Deserialize)]
struct ChatReq {
    messages: Vec<ChatMsg>,
    #[serde(default)]
    model: Option<String>, // OpenAI field: a model name or volume from /models; absent (or unknown) = the largest
    #[serde(default)]
    target: Option<String>, // Enclave extension: cpu | gpu | auto
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    max_completion_tokens: Option<usize>, // newer OpenAI name
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    top_p: Option<f32>,
    #[serde(default)]
    top_k: Option<usize>, // extension (common in OSS servers)
    #[serde(default)]
    stream: Option<bool>,
    #[serde(default)]
    stop: Option<serde_json::Value>, // string or [string]
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

/// Render + tokenize the conversation; drops oldest turns until it fits.
/// A `system` message in the request overrides the configured default.
fn build_prompt(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
) -> Result<(Vec<u32>, Vec<String>), String> {
    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone())
        .unwrap_or_else(|| cfg.system_prompt.clone());
    let mut msgs: Vec<(String, String)> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| (m.role.clone(), m.content.clone()))
        .collect();
    if msgs.is_empty() {
        return Err("no user/assistant messages".into());
    }
    loop {
        let rendered = config::render_template(&cfg.template, &system, &msgs)?;
        let enc = tok
            .encode(rendered.prompt.as_str(), true)
            .map_err(|e| format!("tokenize: {e}"))?;
        let ids = enc.get_ids().to_vec();
        if ids.len() <= cfg.max_prompt_tokens || msgs.len() <= 1 {
            if ids.len() > cfg.max_prompt_tokens {
                return Err(format!(
                    "message too long: {} tokens (limit {})",
                    ids.len(),
                    cfg.max_prompt_tokens
                ));
            }
            return Ok((ids, rendered.stop_strings));
        }
        msgs.remove(0); // drop the oldest turn and retry
    }
}

fn gen_params(cfg: &AppConfig, creq: &ChatReq, extra_stops: Vec<String>) -> GenParams {
    let mut stops = extra_stops;
    match &creq.stop {
        Some(serde_json::Value::String(s)) if !s.is_empty() => stops.push(s.clone()),
        Some(serde_json::Value::Array(a)) => {
            for v in a.iter().take(4) {
                if let Some(s) = v.as_str() {
                    stops.push(s.to_string());
                }
            }
        }
        _ => {}
    }
    GenParams {
        max_new: creq
            .max_tokens
            .or(creq.max_completion_tokens)
            .unwrap_or(cfg.default_max_new)
            .min(cfg.max_new_cap)
            .max(1),
        sample: SampleParams {
            temperature: creq.temperature.unwrap_or(0.7).clamp(0.0, 2.0),
            top_p: creq.top_p.unwrap_or(0.9).clamp(0.05, 1.0),
            top_k: creq.top_k.unwrap_or(0),
            rep_penalty: cfg.rep_penalty,
            rep_window: cfg.rep_window,
        },
        stop_strings: stops,
    }
}

fn targets_for(mode: &str) -> Vec<(ExecutionTarget, &'static str)> {
    match mode {
        "cpu" => vec![(ExecutionTarget::Cpu, "cpu")],
        "gpu" => vec![(ExecutionTarget::Gpu, "gpu")],
        _ => vec![(ExecutionTarget::Gpu, "gpu"), (ExecutionTarget::Cpu, "cpu")],
    }
}

fn respond_bytes(out: ResponseOutparam, status: u16, ctype: &str, body_bytes: &[u8]) {
    respond_with_cache(out, status, ctype, body_bytes, None)
}

/// Static assets get a long immutable cache: the font never changes within a
/// published version, and a redeploy serves from a new origin anyway.
fn respond_asset(out: ResponseOutparam, ctype: &str, body_bytes: &[u8]) {
    respond_with_cache(out, 200, ctype, body_bytes, Some("public, max-age=31536000, immutable"))
}

fn respond_with_cache(
    out: ResponseOutparam,
    status: u16,
    ctype: &str,
    body_bytes: &[u8],
    cache: Option<&str>,
) {
    let headers = Fields::new();
    let _ = headers.set(&"content-type".to_string(), &[ctype.as_bytes().to_vec()]);
    if let Some(c) = cache {
        let _ = headers.set(&"cache-control".to_string(), &[c.as_bytes().to_vec()]);
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

/// Bearer-token check for /v1/*. No key configured = open (gate with a
/// private deployment instead when that is the intent).
fn authorized(cfg: &AppConfig, req: &IncomingRequest) -> bool {
    let Some(key) = &cfg.api_key else { return true };
    let headers = req.headers();
    for v in headers.get(&"authorization".to_string()) {
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

// ------------------------------------------------- legacy /chat (playground) --

fn handle_chat(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = &match resolve_model(raw, creq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    let tok_bytes = match read_tokenizer(cfg) {
        Ok(b) => b,
        Err(e) => return json_err(out, 500, &e),
    };
    let tok = match Tokenizer::from_bytes(&tok_bytes) {
        Ok(t) => t,
        Err(e) => return json_err(out, 500, &format!("tokenizer: {e}")),
    };
    let (prompt_ids, stops) = match build_prompt(cfg, &tok, &creq.messages) {
        Ok(v) => v,
        Err(e) => return json_err(out, 400, &e),
    };
    let params = gen_params(cfg, &creq, stops);

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

    let mode = creq.target.as_deref().unwrap_or("auto");
    let mut last_err = String::new();
    let mut ok = false;
    for (i, (target, tname)) in targets_for(mode).iter().enumerate() {
        if i > 0 && !send(serde_json::json!({ "notice": format!("gpu failed ({last_err}); retrying on cpu") })) {
            break;
        }
        let emit = |delta: &str| send(serde_json::json!({ "delta": delta }));
        let status = |s: &str| send(serde_json::json!({ "status": s }));
        match generate(cfg, &tok, &prompt_ids, *target, tname, &params, &emit, &status) {
            Ok(s) => {
                let gen_s = (s.decode_ms as f64) / 1000.0;
                let tok_per_s = if gen_s > 0.0 { s.tokens as f64 / gen_s } else { 0.0 };
                send(serde_json::json!({
                    "done": true, "target": s.target,
                    "prompt_tokens": s.prompt_tokens, "tokens": s.tokens,
                    "load_ms": s.load_ms as u64, "prefill_ms": s.prefill_ms as u64,
                    "decode_ms": s.decode_ms as u64,
                    "finish_reason": s.finish_reason,
                    "tok_per_s": (tok_per_s * 10.0).round() / 10.0,
                }));
                ok = true;
                break;
            }
            Err(e) => last_err = format!("{tname}: {e}"),
        }
    }
    if !ok && !last_err.is_empty() {
        send(serde_json::json!({ "error": last_err }));
    }
    drop(stream);
    let _ = OutgoingBody::finish(body, None);
}

// --------------------------------------- OpenAI-compatible /v1 endpoints --

fn completion_id() -> String {
    format!("chatcmpl-enclave{:x}", now_ms())
}

fn handle_completions(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    // auth is deployment policy (top-level api_key), not per-model
    let base = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&base, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let parsed: Result<ChatReq, String> = read_body(&req)
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| format!("bad JSON: {e}")));
    let creq = match parsed {
        Ok(c) => c,
        Err(e) => return json_err(out, 400, &e),
    };
    let cfg = &match resolve_model(raw, creq.model.as_deref()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    let tok_bytes = match read_tokenizer(cfg) {
        Ok(b) => b,
        Err(e) => return json_err(out, 500, &e),
    };
    let tok = match Tokenizer::from_bytes(&tok_bytes) {
        Ok(t) => t,
        Err(e) => return json_err(out, 500, &format!("tokenizer: {e}")),
    };
    let (prompt_ids, stops) = match build_prompt(cfg, &tok, &creq.messages) {
        Ok(v) => v,
        Err(e) => return json_err(out, 400, &e),
    };
    let params = gen_params(cfg, &creq, stops);
    let mode = creq.target.as_deref().unwrap_or("auto");
    let id = completion_id();
    let created = (now_ms() / 1000) as u64;
    let model = cfg.name.clone();

    if creq.stream.unwrap_or(false) {
        // ---- streaming: OpenAI chunk protocol over SSE
        let headers = Fields::new();
        let _ = headers.set(&"content-type".to_string(), &[b"text/event-stream".to_vec()]);
        let _ = headers.set(&"cache-control".to_string(), &[b"no-cache".to_vec()]);
        let resp = OutgoingResponse::new(headers);
        let body = resp.body().unwrap();
        ResponseOutparam::set(out, Ok(resp));
        let stream = body.write().unwrap();
        let send_raw = |s: &str| -> bool {
            for chunk in s.as_bytes().chunks(4000) {
                if stream.blocking_write_and_flush(chunk).is_err() {
                    return false;
                }
            }
            true
        };
        let chunk = |delta: serde_json::Value, finish: Option<&str>| -> String {
            format!(
                "data: {}\n\n",
                serde_json::json!({
                    "id": id, "object": "chat.completion.chunk", "created": created,
                    "model": model,
                    "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
                })
            )
        };
        // role preamble chunk (OpenAI clients expect it)
        let _ = send_raw(&chunk(serde_json::json!({ "role": "assistant" }), None));

        let mut last_err = String::new();
        let mut done_stats: Option<GenStats> = None;
        for (target, tname) in targets_for(mode).iter() {
            let emit = |delta: &str| send_raw(&chunk(serde_json::json!({ "content": delta }), None));
            // OpenAI protocol has no status events; SSE comments keep the
            // connection warm through cold session init without confusing SDKs
            let status = |s: &str| send_raw(&format!(": {s}\n\n"));
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, &emit, &status) {
                Ok(s) => {
                    done_stats = Some(s);
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        match done_stats {
            Some(s) => {
                let _ = send_raw(&chunk(serde_json::json!({}), Some(s.finish_reason)));
                let _ = send_raw("data: [DONE]\n\n");
            }
            None => {
                let _ = send_raw(&format!(
                    "data: {}\n\n",
                    serde_json::json!({ "error": { "message": last_err, "type": "server_error" } })
                ));
            }
        }
        drop(stream);
        let _ = OutgoingBody::finish(body, None);
    } else {
        // ---- non-streaming: run to completion, one JSON response
        let sink = |_: &str| true;
        let mut last_err = String::new();
        let mut result: Option<GenStats> = None;
        for (target, tname) in targets_for(mode).iter() {
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, &sink, &sink) {
                Ok(s) => {
                    result = Some(s);
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        match result {
            Some(s) => {
                let body_json = serde_json::json!({
                    "id": id, "object": "chat.completion", "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": s.text },
                        "finish_reason": s.finish_reason,
                    }],
                    "usage": {
                        "prompt_tokens": s.prompt_tokens,
                        "completion_tokens": s.tokens,
                        "total_tokens": s.prompt_tokens + s.tokens,
                    },
                    "enclave": { "target": s.target, "load_ms": s.load_ms as u64,
                             "prefill_ms": s.prefill_ms as u64, "decode_ms": s.decode_ms as u64 },
                });
                respond_bytes(out, 200, "application/json", body_json.to_string().as_bytes());
            }
            None => json_err(out, 500, &last_err),
        }
    }
}

// ------------------------------------------------------------------ warmup --

/// Warm one model: open a session and feed a single in-vocab token, which
/// forces the full compute path (workspace allocation, kernel warm) without
/// generating anything a user could see. Returns (target, load_ms, feed_ms).
/// Repeat calls are cheap (the load coalesces on the host's session cache /
/// preloaded graph). An error here IS the fit signal the ladder consumes:
/// an absent host graph, a failed context/KV allocation and a failed first
/// compute all mean this model does not serve under the current share.
fn warm_one(cfg: &AppConfig, mode: &str) -> Result<(String, u64, u64), String> {
    let warm_tok = cfg.eos.first().copied().unwrap_or(0);
    let mut last_err = String::new();
    for (target, tname) in targets_for(mode) {
        let t0 = now_ms();
        let opened = Session::open(cfg, target);
        let load_ms = now_ms() - t0;
        let t1 = now_ms();
        match opened.and_then(|mut sess| sess.feed(cfg, &[warm_tok], true)) {
            Ok(_) => return Ok((tname.to_string(), load_ms as u64, (now_ms() - t1) as u64)),
            Err(e) => last_err = format!("{tname}: {e}"),
        }
    }
    Err(last_err)
}

/// GET /warmup - put weights and kernels in device memory BEFORE the first
/// real prompt.
///
/// `?model=<name|volume>` warms that ONE model (the playground re-warms on
/// selection change); response shape and error semantics are the classic
/// single-model ones. BARE `/warmup` is the LADDER: every servable model,
/// SMALLEST FIRST, warmed one at a time, failures recorded and skipped.
/// Smallest-first is deliberate on both axes: residency within the share is
/// first-come-first-served, so the models most likely to fit are resident
/// (and guaranteed) before a bigger sibling claims - or fails to claim -
/// the rest; and one published app degrades gracefully across deployment
/// sizes: a small share serves the small models and reports the big ones
/// unfit, a bigger share unlocks them. The manager's boot warmup GETs the
/// bare path, so a fresh deployment sorts itself out at launch; the
/// playground fires it on page load and disables the unfit models in its
/// menu. 200 with per-model results while at least one model warmed, 500
/// when none did.
///
/// ?target= defaults to GPU ONLY - warmup exists to put weights in VRAM,
/// and a failed GPU should read as a failed warmup, not silently pre-build
/// the CPU session (chat's auto mode still falls back at request time).
/// Pass target=cpu (dev boxes) or target=auto explicitly to warm other
/// paths. Slow by design when cold - the response arrives when the models
/// are ready.
fn handle_warmup(raw: &serde_json::Value, query: &str, out: ResponseOutparam) {
    let mode = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("target="))
        .unwrap_or("gpu");
    let model = query.split('&').find_map(|kv| kv.strip_prefix("model="));

    if model.is_some() {
        // single-model mode; unknown names fall back to the default model
        // (resolve_model semantics), no servable models to the top-level
        // config whose volume-not-attached error says what to attach
        let cfg = &match resolve_model(raw, model) {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        return match warm_one(cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            Err(e) => json_err(out, 500, &e),
        };
    }

    // ladder mode: smallest first (available_models sorts largest-first)
    let mut entries = available_models(raw);
    entries.reverse();
    if entries.is_empty() {
        let cfg = &match resolve_model(raw, None) {
            Ok(c) => c,
            Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
        };
        return match warm_one(cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                });
                respond_bytes(out, 200, "application/json", body.to_string().as_bytes())
            }
            Err(e) => json_err(out, 500, &e),
        };
    }
    // models the VRAM budget certainly cannot hold are reported unfit
    // WITHOUT probing - no point starting a multi-GB load that must OOM.
    // CPU-target warms skip the gate (dev boxes have no VRAM budget).
    let unfit = if mode == "cpu" {
        std::collections::HashMap::new()
    } else {
        over_budget(&entries)
    };
    let mut ladder = Vec::with_capacity(entries.len());
    let mut default: Option<String> = None; // largest warmed = last ok in ascending order
    for e in &entries {
        if let Some(why) = unfit.get(&e.volume) {
            ladder.push(serde_json::json!({
                "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                "ok": false, "skipped": true, "error": why,
            }));
            continue;
        }
        match warm_one(&e.cfg, mode) {
            Ok((target, load_ms, feed_ms)) => {
                default = Some(e.cfg.name.clone());
                ladder.push(serde_json::json!({
                    "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                    "ok": true, "target": target, "load_ms": load_ms, "feed_ms": feed_ms,
                }));
            }
            Err(err) => ladder.push(serde_json::json!({
                "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                "ok": false, "error": err,
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

/// GET /models - the playground's dropdown source: servable models largest
/// (= default) first. Open like the playground itself; volume names are
/// already surfaced by error messages, and weights sizes are public catalog
/// facts, so nothing here needs the API key.
fn handle_model_list(raw: &serde_json::Value, out: ResponseOutparam) {
    let entries = available_models(raw);
    let unfit = over_budget(&entries);
    let body = serde_json::json!({
        "default": entries.first().map(|e| e.cfg.name.clone()),
        "vram_budget": vram_budget(),
        "models": entries.iter().enumerate().map(|(i, e)| {
            let mut m = serde_json::json!({
                "name": e.cfg.name, "volume": e.volume, "backend": e.cfg.backend,
                "bytes": e.bytes, "default": i == 0,
                "fits": !unfit.contains_key(&e.volume),
            });
            if let Some(why) = unfit.get(&e.volume) {
                m["why"] = serde_json::json!(why);
            }
            m
        }).collect::<Vec<_>>(),
    });
    respond_bytes(out, 200, "application/json", body.to_string().as_bytes());
}

fn handle_models(raw: &serde_json::Value, req: IncomingRequest, out: ResponseOutparam) {
    let base = match config::from_value(raw.clone()) {
        Ok(c) => c,
        Err(e) => return json_err(out, 500, &format!("configuration error: {e}")),
    };
    if !authorized(&base, &req) {
        return json_err(out, 401, "missing or invalid API key");
    }
    let entries = available_models(raw);
    let data: Vec<serde_json::Value> = if entries.is_empty() {
        // nothing servable attached: advertise the configured name so SDK
        // flows still see a model id (requests will explain what to attach)
        vec![serde_json::json!({ "id": base.name, "object": "model", "owned_by": "enclave-deployment" })]
    } else {
        entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                serde_json::json!({
                    "id": e.cfg.name, "object": "model", "owned_by": "enclave-deployment",
                    "enclave": { "volume": e.volume, "backend": e.cfg.backend,
                             "bytes": e.bytes, "default": i == 0 },
                })
            })
            .collect()
    };
    let body = serde_json::json!({ "object": "list", "data": data });
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
        match (method, path) {
            (Method::Get, "/") | (Method::Get, "") => {
                respond_bytes(out, 200, "text/html; charset=utf-8", CHAT_HTML.as_bytes())
            }
            (Method::Get, "/emoji.woff2") => respond_asset(out, "font/woff2", EMOJI_WOFF2),
            (Method::Get, "/ping") => respond_bytes(
                out,
                200,
                "application/json",
                format!("{{\"ok\":true,\"pong\":true,\"t\":{}}}", now_ms()).as_bytes(),
            ),
            (Method::Get, "/models") => handle_model_list(&raw, out),
            (Method::Get, "/warmup") => handle_warmup(&raw, query, out),
            (Method::Post, "/chat") => handle_chat(&raw, req, out),
            (Method::Post, "/v1/chat/completions") => handle_completions(&raw, req, out),
            (Method::Get, "/v1/models") => handle_models(&raw, req, out),
            _ => json_err(
                out,
                404,
                "not found; routes: GET /, GET /emoji.woff2, GET /ping, GET /models, GET /warmup, GET /v1/models, POST /v1/chat/completions, POST /chat",
            ),
        }
    }
}

bindings::export!(Component with_types_in bindings);
