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
//!                               Also carries `gpu`: whether the platform gave
//!                               this deployment a GPU share (null = not
//!                               knowable here). False means the fleet had no
//!                               GPU enclave free and the app is serving in
//!                               CPU mode - the playground says so on load.
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
//!                               `enable_thinking: false` (top-level or in
//!                               chat_template_kwargs, the vLLM spelling)
//!                               turns off <think> reasoning on models whose
//!                               config marks them `thinking`. Thinking on
//!                               follows the qwen3.x templates: the prompt
//!                               force-opens the block and the server
//!                               re-emits `<think>\n` at the head of the
//!                               reply; history replays drop prior think
//!                               blocks.
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

/// The volume names whose graphs the manager put on the wasmtime cmdline at
/// tenant boot (ENCLAVE_NN_PRELOADS). The graph registry is sealed at process
/// start, so this is the complete list of names load_by_name() can ever find:
/// a NotFound on a listed name means the boot preload FAILED (loudly, in the
/// tenant log); on an unlisted name the host never tried (volume mounted
/// late, over the VRAM budget, or no unambiguous file). None = the manager
/// predates the env - no signal, keep the generic diagnosis.
fn preloaded_graphs() -> Option<Vec<String>> {
    std::env::var("ENCLAVE_NN_PRELOADS").ok().map(|v| {
        v.split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    })
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

/// Whether this deployment holds GPU resources at all - the fact the
/// playground's CPU-mode notice is built on.
///
/// ENCLAVE_VRAM_BYTES is set from gpuShare x card VRAM on every GPU
/// deployment and left unset on a CPU one, so a value is PROOF of a share.
/// The NEGATIVE needs a second witness, because a manager predating the
/// variable also reports nothing and calling that "no GPU" would put the
/// notice in front of users on a perfectly healthy GPU node:
///   * some ENCLAVE_* variable must be in the environment at all - otherwise
///     this is a dev box, not a tenant, and nothing is known;
///   * the host must have preloaded NO ggml graphs - a preload is only
///     possible on a GPU-share deployment (the manager puts the GGUF in
///     VRAM at tenant boot), so ENCLAVE_NN_PRELOADS with entries means an
///     older manager on a GPU node, not a CPU one.
///
/// Some(true) = GPU share; Some(false) = tenant with no GPU, i.e. CPU mode;
/// None = unknowable here, and the playground stays quiet rather than guess.
fn gpu_present() -> Option<bool> {
    if vram_budget().is_some() {
        return Some(true);
    }
    let tenant = std::env::vars().any(|(k, _)| k.starts_with("ENCLAVE_"));
    let preloaded = preloaded_graphs().is_some_and(|p| !p.is_empty());
    (tenant && !preloaded).then_some(false)
}

/// Bytes per KV-cache element, in SIXTEENTHS (q8_0 stores 34 bytes per
/// 32-element block = 17/16 bytes each; f16 = 32/16; and so on).
fn kv_elem_sixteenths(t: &str) -> u64 {
    match t {
        "f32" => 64,
        "q8_0" => 17,
        "q4_0" => 9,
        "q4_1" => 10,
        _ => 32, // f16 / bf16 / unknown
    }
}

/// Estimated VRAM to SERVE one ggml model beyond its resident weights: the
/// KV cache at the node's context window plus a flat working-set allowance
/// (compute buffers, FA workspace, CUDA context). llama.cpp allocates the
/// FULL window up front regardless of prompt length and does NOT clamp to
/// the model's training window, so the window term dominates. The node
/// tuning (ENCLAVE_GGML_N_CTX + KV cache types) is forwarded by the
/// manager; when absent (older managers, dev boxes) this returns 0 and the
/// budget gate degrades to weights-only, as before. Returns
/// (kv_bytes, working_set) - callers sum them.
const WORKING_SET: u64 = 3 << 29; // 1.5 GiB, deliberately round
fn serve_cost(cfg: &AppConfig) -> (u64, u64) {
    if cfg.backend != "ggml" {
        return (0, 0);
    }
    let Some(n_ctx) = std::env::var("ENCLAVE_GGML_N_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
    else {
        return (0, 0);
    };
    let tk = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE").unwrap_or_default();
    let tv = std::env::var("ENCLAVE_GGML_KV_CACHE_TYPE_V").unwrap_or_else(|_| tk.clone());
    let elems = cfg.kv_layers.unwrap_or(cfg.n_layers) as u64
        * cfg.n_kv_heads as u64
        * cfg.head_dim as u64;
    let kv = elems * n_ctx * (kv_elem_sixteenths(tk.trim()) + kv_elem_sixteenths(tv.trim())) / 16;
    if pooled_backend() {
        // Continuous-batching host: ONE shared KV pool of n_ctx tokens per
        // model serves every concurrent session - the pool prices once,
        // regardless of ENCLAVE_GGML_MAX_SESSIONS (that only caps sequences
        // sharing it).
        return (kv, WORKING_SET);
    }
    // Pre-batching host: up to ENCLAVE_GGML_MAX_SESSIONS concurrent contexts,
    // EACH with its own full window + working set - price the worst case.
    // Absent env = 1, matching that host's gate default (and the pre-gate
    // reality only by assumption, which is what broke 2026-07-24).
    let sessions = std::env::var("ENCLAVE_GGML_MAX_SESSIONS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1);
    (kv * sessions, WORKING_SET * sessions)
}

/// The manager sets ENCLAVE_GGML_POOLED on hosts whose ggml backend serves
/// all concurrent sessions from one shared KV pool per model (continuous
/// batching). It changes the serve-cost SHAPE: pools price once per model
/// and PERSIST once a model has served (the host keeps the server for the
/// graph's lifetime), instead of transient per-request windows.
fn pooled_backend() -> bool {
    std::env::var("ENCLAVE_GGML_POOLED")
        .map(|v| !v.trim().is_empty() && v.trim() != "0")
        .unwrap_or(false)
}

/// Which servable models CANNOT serve within the VRAM budget. Models claim
/// the budget smallest-first (the preload / warmup order, same tie-break as
/// the manager's emission), each needing its weights RESIDENT plus - while
/// a session is open - its KV cache at the node's window and a working-set
/// allowance (serve_cost). Refusing here is load-bearing, not cosmetic: a
/// CUDA OOM inside compute ABORTS the whole wasmtime process (ggml_abort -
/// no error reaches the guest, every model goes down with it), so a
/// too-big model must never be probed, let alone served. Serve cost covers
/// the host's whole concurrency model: pooled hosts (ENCLAVE_GGML_POOLED)
/// price one persistent KV pool per model and ACCUMULATE it across models,
/// pre-pooling hosts price ENCLAVE_GGML_MAX_SESSIONS transient windows for
/// the served model only; when the node env is unknown the estimate
/// degrades to weights-only. Returns volume -> reason, unfit models only.
fn over_budget(entries: &[ModelEntry]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let Some(budget) = vram_budget() else { return out };
    let mut asc: Vec<&ModelEntry> = entries.iter().collect();
    asc.sort_by(|a, b| a.bytes.cmp(&b.bytes).then(a.volume.cmp(&b.volume)));
    let gb = |b: u64| b as f64 / (1u64 << 30) as f64;
    let mut claimed = 0u64;
    for e in asc {
        let (kv, ws) = serve_cost(&e.cfg);
        let fits = claimed + e.bytes + kv + ws <= budget;
        if fits {
            // weights stay resident once preloaded; on a pooled host the KV
            // pool and working set ALSO persist from the moment a model first
            // serves (the boot warmup ladder touches every fitting model), so
            // they claim budget from the next model too
            claimed += e.bytes + if pooled_backend() { kv + ws } else { 0 };
            continue;
        }
        {
            let need = e.bytes + kv + ws;
            out.insert(
                e.volume.clone(),
                if kv > 0 {
                    format!(
                        "needs ~{:.1} GB to serve ({:.1} GB weights + {:.1} GB KV cache at the \
                         node's context window + working set) but {:.1} GB of the {:.1} GB VRAM \
                         budget remains - redeploy with a larger GPU share to unlock this model",
                        gb(need),
                        gb(e.bytes),
                        gb(kv),
                        gb(budget.saturating_sub(claimed)),
                        gb(budget)
                    )
                } else {
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
                    )
                },
            );
        }
    }
    out
}

/// The AppConfig serving one request. `requested` (the OpenAI `model` field,
/// or ?model= on /warmup) matches a model name or volume name; an UNKNOWN
/// name falls back to the default model instead of erroring - OpenAI SDKs
/// require a model string and clients routinely send one this deployment
/// never heard of. A KNOWN name the VRAM budget cannot serve is REFUSED
/// with the reason (attempting it can abort the whole tenant - see
/// over_budget); the default pick is the largest attached model that FITS,
/// falling back to the plain largest when nothing fits or the budget is
/// unknown. No servable models at all falls back to the top-level config,
/// whose volume-not-attached error path tells the operator what to attach.
fn resolve_model(raw: &serde_json::Value, requested: Option<&str>) -> Result<AppConfig, String> {
    let entries = available_models(raw);
    let unfit = over_budget(&entries);
    if let Some(want) = requested {
        if let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) {
            if let Some(why) = unfit.get(&e.volume) {
                return Err(format!("model '{want}' cannot serve on this deployment: {why}"));
            }
            return Ok(e.cfg.clone());
        }
    }
    if let Some(e) = entries.iter().find(|e| !unfit.contains_key(&e.volume)) {
        return Ok(e.cfg.clone());
    }
    match entries.into_iter().next() {
        Some(e) => Ok(e.cfg),
        None => config::from_value(raw.clone()),
    }
}

/// How this generation's tokens get proposed for verification.
enum DraftPlan {
    /// no drafting - plain decode
    Plain,
    /// a same-tokenizer sibling model proposes (its resolved config)
    Model(AppConfig),
    /// the model's own MTP head proposes (host-side; no second model)
    Mtp,
}

/// The draft plan for speculative decoding, when `cfg` names one and it is
/// usable - else Plain plus the reason for a status line. "mtp" = the
/// model's own head (existence verified against the host at session open).
/// For a MODEL draft the tokenizer must be IDENTICAL (byte-equal
/// tokenizer.json): draft token ids are meaningless in a foreign
/// vocabulary, and differing merge tables corrupt silently. Two volumes
/// shipping cosmetically different copies of the same tokenizer can be
/// forced compatible by pointing the draft entry's tokenizer_file at the
/// target's copy (absolute /models/... paths work).
fn resolve_draft(raw: &serde_json::Value, cfg: &AppConfig) -> (DraftPlan, Option<String>) {
    let Some(want) = cfg.draft.as_deref() else { return (DraftPlan::Plain, None) };
    if cfg.backend != "ggml" {
        return (DraftPlan::Plain, Some("speculative decoding needs the ggml backend".into()));
    }
    if want == "mtp" {
        // whether the loaded GGUF actually carries a head is the host's
        // knowledge - probed via caps at session open, which falls back
        // with its own note if not
        return (DraftPlan::Mtp, None);
    }
    let entries = available_models(raw);
    let Some(e) = entries.iter().find(|e| e.cfg.name == want || e.volume == want) else {
        return (DraftPlan::Plain, Some(format!("draft model '{want}' is not attached (or not in the models catalog)")));
    };
    if e.cfg.backend != "ggml" {
        return (DraftPlan::Plain, Some(format!("draft '{want}' is not a ggml model")));
    }
    if e.cfg.vocab != cfg.vocab {
        return (DraftPlan::Plain, Some(format!(
            "draft '{want}' speaks a different vocabulary ({} vs {}) - a draft must share the              target's tokenizer exactly (qwen3.x models share one; qwen2.5-0.5b does not)",
            e.cfg.vocab, cfg.vocab
        )));
    }
    match (read_tokenizer(cfg), read_tokenizer(&e.cfg)) {
        (Ok(a), Ok(b)) if a == b => {}
        (Ok(_), Ok(_)) => {
            return (DraftPlan::Plain, Some(format!(
                "draft '{want}' ships a different tokenizer.json than the target - set the draft                  entry's tokenizer_file to the target's copy (an absolute /models/... path) if                  they are really the same tokenizer"
            )))
        }
        _ => return (DraftPlan::Plain, Some("couldn't read both tokenizers to verify draft compatibility".into())),
    }
    if over_budget(&entries).contains_key(&e.volume) {
        return (DraftPlan::Plain, Some(format!("draft '{want}' does not fit this deployment's share")));
    }
    (DraftPlan::Model(e.cfg.clone()), None)
}

const PREFILL_CHUNK: usize = 128;
const MAX_BODY_BYTES: usize = 256 * 1024;

/// The host's ggml session gate (ENCLAVE_GGML_MAX_SESSIONS) tags its
/// fail-fast error with this marker. Every llama context pre-allocates the
/// FULL n_ctx KV window, so the host caps live contexts and this app QUEUES
/// (wait-and-retry below) instead of stacking window allocations until the
/// share's VRAM dies — the 2026-07-24 many-users outage, which surfaced as
/// "cpu: llama.cpp context allocation failed (n_ctx too large for the
/// share?)" on every request.
const BUSY_MARKER: &str = "[sessions_busy]";
const BUSY_POLL_MS: u64 = 2000;
const BUSY_WAIT_BUDGET_MS: u128 = 300_000; // stop queueing after 5 minutes

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Sleep on the monotonic clock's pollable — parks this request in the host's
/// poll loop (a spin wait would burn the share's cpu slice for nothing).
fn sleep_ms(ms: u64) {
    let p = bindings::wasi::clocks::monotonic_clock::subscribe_duration(ms * 1_000_000);
    bindings::wasi::io::poll::poll(&[&p]);
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

/// Diagnose a failed ggml load_by_name(). A NotFound is NOT one condition -
/// the graph registry seals at process start, so it splits three ways, and
/// telling users the wrong one ("volume missing" for a model the platform is
/// about to load) was exactly the confusion seen live 2026-07-18:
///   volume dir absent        -> operator error: attach the volume
///   name in ENCLAVE_NN_PRELOADS -> the boot preload FAILED (tenant log)
///   name not in it           -> the host never tried: mounted late (the
///                               platform detects and restarts) or over the
///                               VRAM budget (the ladder marks it unfit)
/// The [code] marker rides in the message; json_err lifts it into the error
/// object's machine-readable `code` and strips it from the text.
fn ggml_load_err(cfg: &AppConfig, e: bindings::wasi::nn::errors::Error) -> String {
    use bindings::wasi::nn::errors::ErrorCode;
    let not_found = matches!(e.code(), ErrorCode::NotFound);
    let base = nn_err("load_by_name", e);
    let vol = &cfg.model_volume;
    if !not_found {
        return format!("{base} (loading \"{vol}\")");
    }
    if !PathBuf::from(MODELS_ROOT).join(vol).is_dir() {
        return format!(
            "[volume_not_attached] {base} - the \"{vol}\" volume is not attached; deploy \
             with {{\"volumes\":[\"{vol}\"]}} in the config, or tick it in the console's \
             volume picker"
        );
    }
    match preloaded_graphs() {
        Some(pre) if pre.iter().any(|p| p == vol) => format!(
            "[host_load_failed] {base} - the host tried to load \"{vol}\" when this \
             deployment started and FAILED (the deployment log has the reason); if this \
             persists, the deployment's share cannot hold the model"
        ),
        Some(_) => format!(
            "[model_not_loaded] {base} - \"{vol}\" is attached but was not loaded when \
             this deployment started (the volume finished mounting later, or it exceeds \
             the share's VRAM budget); the platform restarts the deployment to load it - \
             retry shortly"
        ),
        None => format!(
            "{base} (is the \"{vol}\" volume attached, and does it carry a GGUF? \
             ggml needs a GPU-share deployment - the host preloads the model)"
        ),
    }
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
                let graph = load_by_name(&cfg.model_volume).map_err(|e| ggml_load_err(cfg, e))?;
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

impl Session {
    /// ggml only: feed `ids` and get EVERY position's logits row back
    /// (dims [n, vocab]) - the speculative verify pass: the target consumes
    /// the draft's proposals in ONE forward pass.
    fn feed_all(&mut self, cfg: &AppConfig, ids: &[u32]) -> Result<Vec<Vec<f32>>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("all".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
            ])
            .map_err(|e| nn_err("compute", e))?;
        let logits = outs
            .iter()
            .find(|(n, _)| n == "logits")
            .ok_or("ggml backend returned no \"logits\" output")?;
        let data = logits.1.data();
        let row = cfg.vocab * 4;
        if data.len() != row * ids.len() {
            return Err(format!(
                "expected {} logit rows of {} bytes, got {} bytes - the host predates \
                 speculative decoding? (per-position logits need the spec toolchain)",
                ids.len(), row, data.len()
            ));
        }
        Ok(data
            .chunks_exact(row)
            .map(|r| r.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
            .collect())
    }

    /// ggml only: this session's capabilities - (seq_id, recurrent, mtp).
    /// seq_id is the handle another session on the SAME graph names to
    /// branch from it (`copy_from`); mtp = the loaded GGUF carries a
    /// multi-token-prediction head. Errors on hosts that predate the
    /// speculative toolchain, which is the capability probe: no caps, no
    /// speculative decode. Hosts before the MTP toolchain return two
    /// values - a missing third reads as "no head".
    fn caps(&mut self) -> Result<(i32, bool, bool), String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let outs = ctx
            .compute(vec![(
                "caps".to_string(),
                Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes()),
            )])
            .map_err(|e| nn_err("caps", e))?;
        let caps = outs
            .iter()
            .find(|(n, _)| n == "caps")
            .ok_or("host returned no \"caps\" output")?;
        let data = caps.1.data();
        if data.len() < 8 {
            return Err("caps output too short".into());
        }
        let v = |i: usize| {
            i32::from_le_bytes([data[i * 4], data[i * 4 + 1], data[i * 4 + 2], data[i * 4 + 3]])
        };
        let mtp = data.len() >= 12 && v(2) != 0;
        Ok((v(0), v(1) != 0, mtp))
    }

    /// ggml only: MTP-aware feed of this sequence - the host runs an
    /// all-positions pass, mirrors every position into the model's own MTP
    /// head, and returns only the LAST logits row. The speculative prefill.
    fn feed_mtp(&mut self, cfg: &AppConfig, ids: &[u32]) -> Result<Vec<f32>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("mtp".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
            ])
            .map_err(|e| nn_err("compute", e))?;
        let logits = outs
            .iter()
            .find(|(n, _)| n == "logits")
            .ok_or("ggml backend returned no \"logits\" output")?;
        let data = logits.1.data();
        if data.len() != cfg.vocab * 4 {
            return Err(format!(
                "mtp feed returned {} bytes, config vocab says {}",
                data.len(),
                cfg.vocab * 4
            ));
        }
        Ok(data
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect())
    }

    /// ggml only: verify pass that ALSO harvests the target's MTP hidden
    /// rows for `real_seq` (the verify runs on a scratch branch, but the
    /// head state belongs to the real sequence).
    fn feed_all_mtp(
        &mut self,
        cfg: &AppConfig,
        ids: &[u32],
        real_seq: i32,
    ) -> Result<Vec<Vec<f32>>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let bytes: Vec<u8> = ids.iter().flat_map(|&t| (t as i32).to_le_bytes()).collect();
        let outs = ctx
            .compute(vec![
                ("tokens".to_string(), Tensor::new(&[1, ids.len() as u32], TensorType::I32, &bytes)),
                ("all".to_string(), Tensor::new(&[1], TensorType::I32, &1i32.to_le_bytes())),
                ("mtp_for".to_string(), Tensor::new(&[1], TensorType::I32, &real_seq.to_le_bytes())),
            ])
            .map_err(|e| nn_err("compute", e))?;
        let logits = outs
            .iter()
            .find(|(n, _)| n == "logits")
            .ok_or("ggml backend returned no \"logits\" output")?;
        let data = logits.1.data();
        let row = cfg.vocab * 4;
        if data.len() != row * ids.len() {
            return Err(format!(
                "expected {} logit rows of {} bytes, got {} bytes",
                ids.len(), row, data.len()
            ));
        }
        Ok(data
            .chunks_exact(row)
            .map(|r| r.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
            .collect())
    }

    /// ggml only: the model's own MTP head proposes up to k tokens (greedy,
    /// stops when its confidence drops below p_min). May return fewer or none.
    fn mtp_draft(&mut self, id_last: u32, k: usize, p_min_milli: i32) -> Result<Vec<u32>, String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(12);
        bytes.extend_from_slice(&(id_last as i32).to_le_bytes());
        bytes.extend_from_slice(&(k as i32).to_le_bytes());
        bytes.extend_from_slice(&p_min_milli.to_le_bytes());
        let outs = ctx
            .compute(vec![(
                "mtp_draft".to_string(),
                Tensor::new(&[3], TensorType::I32, &bytes),
            )])
            .map_err(|e| nn_err("mtp_draft", e))?;
        let draft = outs
            .iter()
            .find(|(n, _)| n == "draft")
            .ok_or("host returned no \"draft\" output")?;
        Ok(draft
            .1
            .data()
            .chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as u32)
            .collect())
    }

    /// ggml only: mirror a verify round's accepted tokens into the MTP head
    /// (pairs them with the rows harvested by the last mtp-flagged pass).
    fn mtp_accept(&mut self, pos0: usize, tokens: &[u32]) -> Result<(), String> {
        let Session::Ggml { ctx } = self else {
            return Err("speculative decoding needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(4 + tokens.len() * 4);
        bytes.extend_from_slice(&(pos0 as i32).to_le_bytes());
        for &t in tokens {
            bytes.extend_from_slice(&(t as i32).to_le_bytes());
        }
        ctx.compute(vec![(
            "mtp_accept".to_string(),
            Tensor::new(&[(1 + tokens.len()) as u32], TensorType::I32, &bytes),
        )])
        .map_err(|e| nn_err("mtp_accept", e))?;
        Ok(())
    }

    /// ggml only: make THIS session an exact branch of `src_seq` (a sibling
    /// session on the same graph, at `src_fed` fed tokens). Attention KV is
    /// shared cells; recurrent state is copy-on-write - branching is free
    /// until this branch decodes. The speculative primitive that replaces
    /// rewind: verify draft tokens on a branch, adopt the branch on full
    /// accept, re-feed only the accepted tokens on partial accept.
    fn copy_from(&mut self, src_seq: i32, src_fed: usize) -> Result<(), String> {
        let Session::Ggml { ctx } = self else {
            return Err("copy_from needs the ggml backend".into());
        };
        let mut bytes = Vec::with_capacity(8);
        bytes.extend_from_slice(&src_seq.to_le_bytes());
        bytes.extend_from_slice(&(src_fed as i32).to_le_bytes());
        ctx.compute(vec![(
            "copy_from".to_string(),
            Tensor::new(&[2], TensorType::I32, &bytes),
        )])
        .map_err(|e| nn_err("copy_from", e))?;
        Ok(())
    }
}

/// Everything speculative decoding needs beyond the target session: the
/// draft session plus one scratch branch per model, with the sequence ids
/// to branch from / adopt into. Opened non-retrying: a busy slot or a host
/// without the speculative verbs degrades to plain decode - drafting is an
/// accelerator, never a dependency.
struct SpecRig {
    dsess: Session,
    tscr: Session,
    dscr: Session,
    t_seq: i32,
    d_seq: i32,
    tscr_seq: i32,
    dscr_seq: i32,
}

fn open_spec(
    cfg: &AppConfig,
    dcfg: &AppConfig,
    target: ExecutionTarget,
    sess: &mut Session,
) -> Result<SpecRig, String> {
    let (t_seq, _, _) = sess.caps()?; // also the host capability probe
    let mut dsess = Session::open(dcfg, target)?;
    let (d_seq, _, _) = dsess.caps()?;
    let mut tscr = Session::open(cfg, target)?;
    let (tscr_seq, _, _) = tscr.caps()?;
    let mut dscr = Session::open(dcfg, target)?;
    let (dscr_seq, _, _) = dscr.caps()?;
    Ok(SpecRig { dsess, tscr, dscr, t_seq, d_seq, tscr_seq, dscr_seq })
}

/// The MTP rig: just the target scratch branch - the model drafts for
/// itself through the host's head context, so no draft model, no draft
/// sessions, half the slot cost of a model-draft rig.
struct MtpRig {
    tscr: Session,
    t_seq: i32,
    tscr_seq: i32,
}

fn open_mtp(
    cfg: &AppConfig,
    target: ExecutionTarget,
    sess: &mut Session,
) -> Result<MtpRig, String> {
    let (t_seq, _, mtp) = sess.caps()?; // also the host capability probe
    if !mtp {
        return Err("this model volume carries no MTP head (use an *-mtp volume, or name a draft model)".into());
    }
    let mut tscr = Session::open(cfg, target)?;
    let (tscr_seq, _, _) = tscr.caps()?;
    Ok(MtpRig { tscr, t_seq, tscr_seq })
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
    /// speculative decoding counters (0/0 on the plain path): how many
    /// tokens the draft proposed and how many the target accepted
    drafted: usize,
    accepted: usize,
}

/// The incremental text pipeline shared by the plain and speculative decode
/// loops: push sampled tokens one at a time; it detokenizes the sequence,
/// holds back the longest stop string so one is never partially emitted,
/// sends stable deltas, and reports a stop-string hit.
enum Pushed {
    More,        // keep generating
    Stopped,     // a stop string landed; text is truncated at the match
    Gone,        // client disconnected mid-emit
}
struct TextOut<'a> {
    tok: &'a Tokenizer,
    emit: &'a dyn Fn(&str) -> bool,
    stops: &'a [String],
    holdback: usize,
    generated: Vec<u32>,
    emitted: usize,
    text: String,
}
impl<'a> TextOut<'a> {
    fn new(tok: &'a Tokenizer, emit: &'a dyn Fn(&str) -> bool, stops: &'a [String]) -> TextOut<'a> {
        TextOut {
            tok, emit, stops,
            holdback: stops.iter().map(|s| s.len()).max().unwrap_or(0),
            generated: Vec::new(), emitted: 0, text: String::new(),
        }
    }
    fn push(&mut self, next: u32) -> Pushed {
        self.generated.push(next);
        if let Ok(text) = self.tok.decode(&self.generated, true) {
            if let Some(pos) = self.stops.iter().filter_map(|s| text.find(s.as_str())).min() {
                self.text = text[..pos].to_string();
                if pos > self.emitted {
                    if !(self.emit)(&text[self.emitted..pos]) { return Pushed::Gone; }
                    self.emitted = pos;
                }
                return Pushed::Stopped;
            }
            let visible = text.len().saturating_sub(self.holdback);
            if !text.ends_with('\u{FFFD}') && visible > self.emitted {
                if let Some(delta) = text.get(self.emitted..visible) {
                    if !(self.emit)(delta) { return Pushed::Gone; }
                    self.emitted = visible;
                }
            }
            self.text = text;
        }
        Pushed::More
    }
    /// send whatever the holdback was withholding (end of generation)
    fn flush(&mut self) {
        if self.text.len() > self.emitted {
            if let Some(delta) = self.text.get(self.emitted..) {
                let _ = (self.emit)(delta);
            }
        }
    }
    /// the repetition-penalty window: the freshest `w` sampled tokens,
    /// falling back to the prompt tail before anything is generated
    fn recent(&self, prompt_ids: &[u32], w: usize) -> Vec<u32> {
        if self.generated.is_empty() {
            prompt_ids[prompt_ids.len().saturating_sub(w)..].to_vec()
        } else {
            self.generated[self.generated.len().saturating_sub(w)..].to_vec()
        }
    }
}

/// Run the full completion; `emit` receives text deltas as they stabilize,
/// `status` receives progress lines. Both return false when the client is
/// gone. Status events double as keepalive bytes during the one long silence
/// (cold session init; the host caches sessions). With `draft` set (a
/// same-tokenizer catalog sibling), decoding is SPECULATIVE: the draft
/// proposes draft_tokens ahead, the target verifies them in one pass, and
/// every accepted token skips a full target step; any draft-side failure
/// falls back to plain decode with a status note.
fn generate(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    target: ExecutionTarget,
    tname: &str,
    p: &GenParams,
    draft: &DraftPlan,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    if !status(&format!(
        "loading the model on {tname} - the first request after a node boot initializes the session and can take a while"
    )) {
        return Err("client disconnected".into());
    }
    let t0 = now_ms();
    // The host caps live inference sessions (each pre-allocates the full
    // context window's KV in the share's VRAM) and fails fast when they are
    // all taken - queue politely: status keepalives out, poll every couple of
    // seconds, give up only after a real wait or when the client hangs up.
    let mut sess = loop {
        match Session::open(cfg, target) {
            Ok(s) => break s,
            Err(e) if e.contains(BUSY_MARKER) => {
                if now_ms() - t0 > BUSY_WAIT_BUDGET_MS {
                    return Err(format!(
                        "every inference session stayed busy for {}s - this deployment is at \
                         its concurrent-chat capacity right now; try again in a little while",
                        BUSY_WAIT_BUDGET_MS / 1000
                    ));
                }
                if !status(&format!(
                    "all inference sessions are busy ({}s) - waiting for a free slot",
                    (now_ms() - t0) / 1000
                )) {
                    return Err("client disconnected".into());
                }
                sleep_ms(BUSY_POLL_MS);
            }
            Err(e) => return Err(e),
        }
    };
    let load_ms = now_ms() - t0;

    // speculative path: the rig opening non-retrying - busy slots, an unfit
    // share, a headless volume, or a host predating the speculative verbs
    // all quietly downgrade to plain decode. Drafting is an accelerator,
    // never a dependency.
    match draft {
        DraftPlan::Model(dc) => match open_spec(cfg, dc, target, &mut sess) {
            Ok(rig) => {
                if !status(&format!("session ready ({load_ms} ms); speculative decode via {} - prefilling {} prompt tokens", dc.name, prompt_ids.len())) {
                    return Err("client disconnected".into());
                }
                return generate_spec(cfg, dc, tok, prompt_ids, tname, p, sess, rig, load_ms, emit, status);
            }
            Err(e) => {
                let _ = status(&format!("draft model unavailable ({}); plain decode", strip_code(&e)));
            }
        },
        DraftPlan::Mtp => match open_mtp(cfg, target, &mut sess) {
            Ok(rig) => {
                if !status(&format!("session ready ({load_ms} ms); speculative decode via the model's MTP head - prefilling {} prompt tokens", prompt_ids.len())) {
                    return Err("client disconnected".into());
                }
                return generate_mtp(cfg, tok, prompt_ids, tname, p, sess, rig, load_ms, emit, status);
            }
            Err(e) => {
                let _ = status(&format!("MTP drafting unavailable ({}); plain decode", strip_code(&e)));
            }
        },
        DraftPlan::Plain => {}
    }
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
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut finish: &'static str = "stop";
    loop {
        let recent = out.recent(prompt_ids, p.sample.rep_window);
        let next = pick_token(&mut logits, &recent, &p.sample, &mut rng);
        if cfg.eos.contains(&next) {
            break;
        }
        if out.generated.len() >= p.max_new {
            finish = "length";
            break;
        }
        match out.push(next) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted: 0, accepted: 0,
                });
            }
            Pushed::Gone => break, // client disconnected
        }
        logits = sess.feed(cfg, &[next], true)?;
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted: 0, accepted: 0,
    })
}

/// Speculative decode: EXACT-MATCH verification. Every emitted token is
/// sampled by the TARGET's own sampler on the target's logits (identical
/// distribution to plain decode - at temperature 0, identical tokens); the
/// draft only predicts what that sampler will say, and each correct
/// prediction folds a full target step into the shared verify pass. Per
/// round: the draft proposes k tokens one cheap step at a time, the target
/// consumes [pending, d1..dk] in ONE pass returning every position's row,
/// and acceptance walks the rows until the target's sample disagrees - the
/// disagreeing sample itself becomes the next token (nothing is wasted).
///
/// BRANCH-COMMIT, not rewind: proposals and the verify pass run on SCRATCH
/// branches (copy_from - shared attention cells, copy-on-write recurrent
/// state), never on the real sequences. A full accept ADOPTS the branch
/// (free); a partial accept re-feeds only the accepted tokens on the real
/// sequence (one batched pass) and abandons the branch. The real sequences
/// never contain a rejected token, so nothing ever needs removal - which is
/// what makes speculation work on hybrid-SSM models (qwen3.5/3.6), whose
/// recurrent state keeps no per-token history and cannot rewind. The
/// alternative (llama.cpp n_rs_seq state snapshots) would cost
/// n_seq_max * state_size per token of depth - ~1.2 GB per token on the
/// 27b at 8 slots - where branching costs zero.
#[allow(clippy::too_many_arguments)]
fn generate_spec(
    cfg: &AppConfig,
    dcfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    tname: &str,
    p: &GenParams,
    mut sess: Session,
    rig: SpecRig,
    load_ms: u128,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let SpecRig { mut dsess, mut tscr, mut dscr, t_seq, d_seq, tscr_seq, dscr_seq } = rig;
    let _ = status; // notes are emitted by the caller; deltas keep the stream warm
    let k = dcfg.draft_tokens.clamp(1, 16).min(cfg.draft_tokens.clamp(1, 16));
    let t1 = now_ms();
    // prefill BOTH models on the prompt; only the target's last row is needed
    let mut done = 0usize;
    let mut t_logits = Vec::new();
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let last = end == prompt_ids.len();
        let l = sess.feed(cfg, &prompt_ids[done..end], last)?;
        if last { t_logits = l; }
        done = end;
    }
    done = 0;
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        dsess.feed(dcfg, &prompt_ids[done..end], false)?;
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut finish: &'static str = "stop";
    let (mut drafted, mut accepted) = (0usize, 0usize);
    // fed-token cursors, so rewinds land on absolute positions
    let mut t_fed = prompt_ids.len();
    let mut d_fed = prompt_ids.len();
    let mut d_behind: Vec<u32> = Vec::new(); // target-fed tokens the draft hasn't seen yet

    // the first token comes straight off the target's prefill row
    let recent = out.recent(prompt_ids, p.sample.rep_window);
    let mut pending = pick_token(&mut t_logits, &recent, &p.sample, &mut rng);
    'outer: loop {
        if cfg.eos.contains(&pending) { break; }
        if out.generated.len() >= p.max_new { finish = "length"; break; }
        match out.push(pending) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted, accepted,
                });
            }
            Pushed::Gone => break 'outer,
        }
        // -- catch the draft's REAL sequence up on accepted history (its
        //    branch only ever carries proposals), then branch it
        let mut catchup = std::mem::take(&mut d_behind);
        catchup.push(pending);
        let mut d_row = dsess.feed(dcfg, &catchup, true)?;
        d_fed += catchup.len();
        dscr.copy_from(d_seq, d_fed)?;
        let mut dscr_fed = d_fed;
        // -- k proposals, one cheap step each, on the draft BRANCH
        let mut drafts: Vec<u32> = Vec::with_capacity(k);
        for i in 0..k {
            let mut rec = out.recent(prompt_ids, p.sample.rep_window);
            rec.extend_from_slice(&drafts);
            let rec = rec[rec.len().saturating_sub(p.sample.rep_window)..].to_vec();
            let d = pick_token(&mut d_row, &rec, &p.sample, &mut rng);
            drafts.push(d);
            if i + 1 < k {
                d_row = dscr.feed(dcfg, &[d], true)?;
                dscr_fed += 1;
            }
        }
        drafted += drafts.len();
        // -- ONE verify pass over [pending, d1..dk] on the target BRANCH:
        //    k+1 rows, the real sequence untouched
        tscr.copy_from(t_seq, t_fed)?;
        let mut feed: Vec<u32> = Vec::with_capacity(k + 1);
        feed.push(pending);
        feed.extend_from_slice(&drafts);
        let mut rows = tscr.feed_all(cfg, &feed)?;
        let tscr_fed = t_fed + feed.len();
        // -- verify: accept while the target's own sample agrees. On every
        //    exit below the generation is OVER - the real sequences are
        //    simply dropped, so no cleanup of the branch is ever needed.
        let mut acc = 0usize;
        let mut replacement: Option<u32> = None;
        for (i, &d) in drafts.iter().enumerate() {
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let expect = pick_token(&mut rows[i], &recent, &p.sample, &mut rng);
            if expect != d {
                replacement = Some(expect);
                break;
            }
            // the draft predicted the target's sample exactly - it is the
            // target's token in every sense; run it through the same gates
            if cfg.eos.contains(&d) {
                accepted += acc;
                break 'outer;
            }
            if out.generated.len() >= p.max_new {
                finish = "length";
                accepted += acc;
                break 'outer;
            }
            match out.push(d) {
                Pushed::More => {}
                Pushed::Stopped => {
                    let decode_ms = now_ms() - t2;
                    return Ok(GenStats {
                        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                        finish_reason: "stop", text: out.text, drafted, accepted: accepted + acc + 1,
                    });
                }
                Pushed::Gone => { accepted += acc + 1; break 'outer; }
            }
            acc += 1;
        }
        accepted += acc;
        if let Some(r) = replacement {
            // partial round: commit ONLY the accepted tokens to the real
            // target (one batched pass, logits unread) and abandon both
            // branches - they are re-branched fresh next round. The draft's
            // real sequence gets the accepted drafts via next round's
            // catchup.
            sess.feed(cfg, &feed[..acc + 1], false)?;
            t_fed += acc + 1;
            d_behind = drafts[..acc].to_vec();
            pending = r;
        } else {
            // full acceptance: ADOPT both branches (attention cells shared,
            // recurrent state referenced - zero decode cost), bonus token
            // from the last row; the draft still owes itself d_k
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            dsess.copy_from(dscr_seq, dscr_fed)?;
            d_fed = dscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_token(&mut rows[last], &recent, &p.sample, &mut rng);
            d_behind.push(drafts[drafts.len() - 1]);
        }
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted, accepted,
    })
}

/// MTP speculative decode: the branch-commit loop of generate_spec with the
/// model's OWN multi-token-prediction head as the proposer - no draft
/// sessions, no second model. Per round the host's head proposes up to k
/// tokens at near-zero cost (it rides hidden state harvested from prior
/// passes), the target verifies them on a scratch branch in one pass, and
/// only ACCEPTED tokens are mirrored back into the head, so its state stays
/// clean. Verification is EXACT-MATCH on the target's own sampler - output
/// is byte-for-byte plain decode, the head only changes speed.
#[allow(clippy::too_many_arguments)]
fn generate_mtp(
    cfg: &AppConfig,
    tok: &Tokenizer,
    prompt_ids: &[u32],
    tname: &str,
    p: &GenParams,
    mut sess: Session,
    rig: MtpRig,
    load_ms: u128,
    emit: &dyn Fn(&str) -> bool,
    status: &dyn Fn(&str) -> bool,
) -> Result<GenStats, String> {
    let _ = status;
    let MtpRig { mut tscr, t_seq, tscr_seq } = rig;
    let k = cfg.draft_tokens.clamp(1, 16);
    let p_min_milli = (cfg.draft_p_min.clamp(0.05, 0.95) * 1000.0) as i32;
    // -- prefill through the MTP-aware feed: every chunk's positions are
    //    mirrored into the head, only last-row logits cross to the guest
    let t1 = now_ms();
    let mut done = 0usize;
    let mut t_logits = Vec::new();
    while done < prompt_ids.len() {
        let end = (done + PREFILL_CHUNK).min(prompt_ids.len());
        let l = sess.feed_mtp(cfg, &prompt_ids[done..end])?;
        if end == prompt_ids.len() { t_logits = l; }
        done = end;
    }
    let prefill_ms = now_ms() - t1;

    let t2 = now_ms();
    let mut rng = Rng::new(now_ms() as u64 ^ (prompt_ids.len() as u64) << 17);
    let mut out = TextOut::new(tok, emit, &p.stop_strings);
    let mut finish: &'static str = "stop";
    let (mut drafted, mut accepted) = (0usize, 0usize);
    let mut t_fed = prompt_ids.len();

    let recent = out.recent(prompt_ids, p.sample.rep_window);
    let mut pending = pick_token(&mut t_logits, &recent, &p.sample, &mut rng);
    'outer: loop {
        if cfg.eos.contains(&pending) { break; }
        if out.generated.len() >= p.max_new { finish = "length"; break; }
        match out.push(pending) {
            Pushed::More => {}
            Pushed::Stopped => {
                let decode_ms = now_ms() - t2;
                return Ok(GenStats {
                    target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                    tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                    finish_reason: "stop", text: out.text, drafted, accepted,
                });
            }
            Pushed::Gone => break 'outer,
        }
        // -- the head proposes (0..=k tokens; an empty draft still verifies
        //    [pending] alone, which adopts as a plain step)
        let drafts = sess.mtp_draft(pending, k, p_min_milli)?;
        drafted += drafts.len();
        // -- ONE verify pass on the target branch, harvesting head rows for
        //    the REAL sequence
        tscr.copy_from(t_seq, t_fed)?;
        let mut feed: Vec<u32> = Vec::with_capacity(drafts.len() + 1);
        feed.push(pending);
        feed.extend_from_slice(&drafts);
        let mut rows = tscr.feed_all_mtp(cfg, &feed, t_seq)?;
        let tscr_fed = t_fed + feed.len();
        // -- verify: accept while the target's own sample agrees
        let mut acc = 0usize;
        let mut replacement: Option<u32> = None;
        for (i, &d) in drafts.iter().enumerate() {
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let expect = pick_token(&mut rows[i], &recent, &p.sample, &mut rng);
            if expect != d {
                replacement = Some(expect);
                break;
            }
            if cfg.eos.contains(&d) {
                accepted += acc;
                break 'outer;
            }
            if out.generated.len() >= p.max_new {
                finish = "length";
                accepted += acc;
                break 'outer;
            }
            match out.push(d) {
                Pushed::More => {}
                Pushed::Stopped => {
                    let decode_ms = now_ms() - t2;
                    return Ok(GenStats {
                        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
                        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
                        finish_reason: "stop", text: out.text, drafted, accepted: accepted + acc + 1,
                    });
                }
                Pushed::Gone => { accepted += acc + 1; break 'outer; }
            }
            acc += 1;
        }
        accepted += acc;
        // -- the head learns ONLY the accepted tokens (its KV never holds a
        //    rejected proposal), then the target commits them
        sess.mtp_accept(t_fed, &feed[..acc + 1])?;
        if let Some(r) = replacement {
            sess.feed(cfg, &feed[..acc + 1], false)?;
            t_fed += acc + 1;
            pending = r;
        } else {
            // full acceptance (also the empty-draft case): adopt the branch
            sess.copy_from(tscr_seq, tscr_fed)?;
            t_fed = tscr_fed;
            let recent = out.recent(prompt_ids, p.sample.rep_window);
            let last = rows.len() - 1;
            pending = pick_token(&mut rows[last], &recent, &p.sample, &mut rng);
        }
    }
    out.flush();
    let decode_ms = now_ms() - t2;
    Ok(GenStats {
        target: tname.to_string(), prompt_tokens: prompt_ids.len(),
        tokens: out.generated.len(), load_ms, prefill_ms, decode_ms,
        finish_reason: finish, text: out.text, drafted, accepted,
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
    #[serde(default)]
    enable_thinking: Option<bool>, // extension: false turns off <think> reasoning (thinking models only)
    #[serde(default)]
    chat_template_kwargs: Option<ChatTemplateKwargs>, // vLLM/SGLang spelling of the same switch
}

#[derive(Deserialize, Default)]
struct ChatTemplateKwargs {
    #[serde(default)]
    enable_thinking: Option<bool>,
}

impl ChatReq {
    /// The request's thinking switch: top-level `enable_thinking` wins over
    /// `chat_template_kwargs.enable_thinking`; absent means on. Only models
    /// whose config marks them `thinking` act on it (see build_prompt).
    fn thinking(&self) -> bool {
        self.enable_thinking
            .or(self.chat_template_kwargs.as_ref().and_then(|k| k.enable_thinking))
            .unwrap_or(true)
    }
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

/// Reasoning is per-turn scratch: a replayed assistant turn keeps only what
/// followed its think block, exactly as the qwen3.x templates render history
/// (split on the last `</think>`, strip leading newlines).
fn strip_think(content: &str) -> String {
    match content.rfind("</think>") {
        Some(i) => content[i + "</think>".len()..].trim_start_matches('\n').to_string(),
        None => content.to_string(),
    }
}

/// Render + tokenize the conversation; drops oldest turns until it fits.
/// A `system` message in the request overrides the configured default.
/// The returned bool is Rendered::think_open: the prompt force-opened a
/// think block that the caller must re-emit in the visible output.
fn build_prompt(
    cfg: &AppConfig,
    tok: &Tokenizer,
    messages: &[ChatMsg],
    thinking: bool, // the request's switch; only cfg.thinking models act on it
) -> Result<(Vec<u32>, Vec<String>, bool), String> {
    let system = messages
        .iter()
        .find(|m| m.role == "system")
        .map(|m| m.content.clone())
        .unwrap_or_else(|| cfg.system_prompt.clone());
    let mut msgs: Vec<(String, String)> = messages
        .iter()
        .filter(|m| m.role == "user" || m.role == "assistant")
        .map(|m| {
            let content = if cfg.thinking && m.role == "assistant" {
                strip_think(&m.content)
            } else {
                m.content.clone()
            };
            (m.role.clone(), content)
        })
        .collect();
    if msgs.is_empty() {
        return Err("no user/assistant messages".into());
    }
    let think = if !cfg.thinking {
        config::ThinkTurn::Plain
    } else if thinking {
        config::ThinkTurn::Open
    } else {
        config::ThinkTurn::Closed
    };
    loop {
        let rendered = config::render_template(&cfg.template, &system, &msgs, think)?;
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
            return Ok((ids, rendered.stop_strings, rendered.think_open));
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

fn targets_for(cfg: &AppConfig, mode: &str) -> Vec<(ExecutionTarget, &'static str)> {
    // ggml ignores the execution target entirely (the model is host-preloaded,
    // device offload is the node env's call), so auto mode's CPU retry would
    // just repeat the SAME failed attempt relabeled "cpu:" — the misleading
    // "cpu: llama.cpp context allocation failed…" users saw live 2026-07-24.
    // One attempt, honestly labeled - and the LABEL follows the deployment:
    // with no GPU share there is nothing to offload to, so the host runs the
    // graph on CPU and saying "gpu" would be a lie in the status line.
    if cfg.backend == "ggml" {
        let label = if gpu_present() == Some(false) { "cpu" } else { "gpu" };
        return vec![(ExecutionTarget::Gpu, label)];
    }
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

/// Machine-readable conditions ggml_load_err() tags inside its messages (as
/// "[code] "); json_err lifts the tag into `error.code` so the playground can
/// render the right state instead of pattern-matching prose.
const ERR_CODES: &[&str] =
    &["model_not_loaded", "host_load_failed", "volume_not_attached", "sessions_busy"];

/// Drop the "[code] " marker from a message bound for a payload without a
/// code field of its own (the chat stream / SSE error lines).
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
    let (prompt_ids, stops, think_open) = match build_prompt(cfg, &tok, &creq.messages, creq.thinking()) {
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
    let (ref draft_cfg, draft_note) = resolve_draft(raw, cfg);
    if let Some(n) = &draft_note {
        let _ = send(serde_json::json!({ "status": format!("speculative decode off: {n}") }));
    }
    let mut last_err = String::new();
    let mut ok = false;
    for (i, (target, tname)) in targets_for(cfg, mode).iter().enumerate() {
        if i > 0 && !send(serde_json::json!({ "notice": format!("gpu failed ({last_err}); retrying on cpu") })) {
            break;
        }
        // the prompt force-opened the think block; re-emit the tag ahead of
        // the first real delta so the client sees a complete block. Lazy,
        // per attempt: a retry notice resets the client's reply buffer, and
        // an attempt that dies before producing output must not leak a tag.
        let opened = std::cell::Cell::new(!think_open);
        let emit = |delta: &str| {
            if !opened.replace(true) && !send(serde_json::json!({ "delta": "<think>\n" })) {
                return false;
            }
            send(serde_json::json!({ "delta": delta }))
        };
        let status = |s: &str| send(serde_json::json!({ "status": s }));
        match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &emit, &status) {
            Ok(s) => {
                let gen_s = (s.decode_ms as f64) / 1000.0;
                let tok_per_s = if gen_s > 0.0 { s.tokens as f64 / gen_s } else { 0.0 };
                let mut done = serde_json::json!({
                    "done": true, "target": s.target,
                    "prompt_tokens": s.prompt_tokens, "tokens": s.tokens,
                    "load_ms": s.load_ms as u64, "prefill_ms": s.prefill_ms as u64,
                    "decode_ms": s.decode_ms as u64,
                    "finish_reason": s.finish_reason,
                    "tok_per_s": (tok_per_s * 10.0).round() / 10.0,
                });
                if s.drafted > 0 {
                    done["draft_tokens"] = serde_json::json!(s.drafted);
                    done["draft_accepted"] = serde_json::json!(s.accepted);
                }
                send(done);
                ok = true;
                break;
            }
            Err(e) => last_err = format!("{tname}: {e}"),
        }
    }
    if !ok && !last_err.is_empty() {
        send(serde_json::json!({ "error": strip_code(&last_err) }));
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
    let (prompt_ids, stops, think_open) = match build_prompt(cfg, &tok, &creq.messages, creq.thinking()) {
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
        let (ref draft_cfg, _) = resolve_draft(raw, cfg);
        for (target, tname) in targets_for(cfg, mode).iter() {
            // re-emit the prompt-side think opening ahead of the first real
            // delta (see handle_chat) so clients receive a complete block
            let opened = std::cell::Cell::new(!think_open);
            let emit = |delta: &str| {
                if !opened.replace(true)
                    && !send_raw(&chunk(serde_json::json!({ "content": "<think>\n" }), None))
                {
                    return false;
                }
                send_raw(&chunk(serde_json::json!({ "content": delta }), None))
            };
            // OpenAI protocol has no status events; SSE comments keep the
            // connection warm through cold session init without confusing SDKs
            let status = |s: &str| send_raw(&format!(": {s}\n\n"));
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &emit, &status) {
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
                    serde_json::json!({ "error": { "message": strip_code(&last_err), "type": "server_error" } })
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
        let (ref draft_cfg, _) = resolve_draft(raw, cfg);
        for (target, tname) in targets_for(cfg, mode).iter() {
            match generate(cfg, &tok, &prompt_ids, *target, tname, &params, draft_cfg, &sink, &sink) {
                Ok(s) => {
                    result = Some(s);
                    break;
                }
                Err(e) => last_err = format!("{tname}: {e}"),
            }
        }
        match result {
            Some(s) => {
                // the prompt force-opened the think block; restore the tag
                // so the reply carries a complete one
                let content =
                    if think_open { format!("<think>\n{}", s.text) } else { s.text.clone() };
                let body_json = serde_json::json!({
                    "id": id, "object": "chat.completion", "created": created,
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": { "role": "assistant", "content": content },
                        "finish_reason": s.finish_reason,
                    }],
                    "usage": {
                        "prompt_tokens": s.prompt_tokens,
                        "completion_tokens": s.tokens,
                        "total_tokens": s.prompt_tokens + s.tokens,
                    },
                    "enclave": { "target": s.target, "load_ms": s.load_ms as u64,
                             "prefill_ms": s.prefill_ms as u64, "decode_ms": s.decode_ms as u64,
                             "draft_tokens": s.drafted, "draft_accepted": s.accepted },
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
    for (target, tname) in targets_for(cfg, mode) {
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
/// the CPU session (chat's auto mode still falls back at request time). The
/// one exception is a deployment with no GPU share at all (gpu_present),
/// where the default drops to auto: there is no VRAM to fail into, and
/// "every model broken" would bury the actual story the playground tells.
/// Pass target=cpu (dev boxes) or target=auto explicitly to warm other
/// paths. Slow by design when cold - the response arrives when the models
/// are ready.
fn handle_warmup(raw: &serde_json::Value, query: &str, out: ResponseOutparam) {
    // GPU by default - but on a deployment the platform gave NO GPU share,
    // "gpu" is a guaranteed failure for the onnx path, and reporting every
    // model broken hides the real story (the fleet had no GPU enclave free).
    // Degrade to auto there, so the CPU session warms and the playground's
    // CPU-mode notice is what the user sees.
    let mode = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("target="))
        .unwrap_or(if gpu_present() == Some(false) { "auto" } else { "gpu" });
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
            // an ACTIVE session holding the slot is proof the model serves -
            // report warm, not failed (a 500 here would make the playground
            // call a busy model unfit and disable it in the picker)
            Err(e) if e.contains(BUSY_MARKER) => {
                let body = serde_json::json!({
                    "ok": true, "model": cfg.name, "volume": cfg.model_volume,
                    "busy": true,
                    "note": "another chat holds the inference session right now; the model is loaded and serving",
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
            // busy = an active chat holds the session slot; the model is
            // loaded and serving, which is exactly what "warm" means here
            Err(err) if err.contains(BUSY_MARKER) => {
                default = Some(e.cfg.name.clone());
                ladder.push(serde_json::json!({
                    "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                    "ok": true, "busy": true,
                }));
            }
            Err(err) => ladder.push(serde_json::json!({
                "model": e.cfg.name, "volume": e.volume, "bytes": e.bytes,
                "ok": false, "error": strip_code(&err),
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
    // ENCLAVE_NN_PRELOADS: which ggml volumes the host actually loaded at
    // boot. `preloaded: false` on a fitting model lets the playground say
    // "waiting for the host" up front instead of probing into a NotFound
    // and calling the model missing. Absent on older managers (no field).
    let preloads = preloaded_graphs();
    let body = serde_json::json!({
        "default": entries.first().map(|e| e.cfg.name.clone()),
        "vram_budget": vram_budget(),
        // true / false / null (unknown) - the playground raises its CPU-mode
        // notice on an explicit false, never on a null
        "gpu": gpu_present(),
        "models": entries.iter().enumerate().map(|(i, e)| {
            let mut m = serde_json::json!({
                "name": e.cfg.name, "volume": e.volume, "backend": e.cfg.backend,
                "bytes": e.bytes, "default": i == 0,
                "fits": !unfit.contains_key(&e.volume),
                "thinking": e.cfg.thinking,
            });
            if let Some(why) = unfit.get(&e.volume) {
                m["why"] = serde_json::json!(why);
            }
            if e.cfg.backend == "ggml" {
                if let Some(pre) = &preloads {
                    m["preloaded"] = serde_json::json!(pre.iter().any(|p| p == &e.volume));
                }
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
